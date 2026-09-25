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
    compute_resumable_model_compatibility,
};
use crate::agent_controller::{
    AgentController, AgentLaunchRequest, AgentRunMode, AgentSurfaceActivity,
};
use crate::child_agent_types::{
    ChildAgentActivityEmitter, ChildAgentActivityPublisher, ChildAgentActivitySink,
    ChildAgentCheckpointObserver, ChildAgentCompatibilityIdentity, ChildAgentContinuationStart,
    SubagentActivityEvent, SubagentActivityIdentity, SubagentActivityOwner,
    SubagentActivityPayload, child_event_output,
};
use crate::child_permission::{ChildPermissionHandler, ChildPermissionIdentity};
use crate::cost::CostTracker;
use crate::execution_scope::Admission;
use crate::hooks::HookRunner;
use crate::instructions::ProjectInstructions;
use crate::lifecycle::{
    RuntimeSessionLifecycle, RuntimeTaskKind, RuntimeTaskLifecycle, RuntimeTaskStatus,
};
use crate::memory::MemoryBlock;
use crate::runtime_permission::RuntimePermissionRequestHandler;
use crate::runtime_surface::RuntimeSubagentActivityIngress;
use crate::runtime_surface::{
    DisplayText, SurfaceSubagentId, SurfaceSubagentTerminalStatus, SurfaceTaskId,
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
    pub(crate) agent_controller: Option<Arc<AgentController>>,
    pub(crate) batch_id: String,
    pub(crate) batch_size: u32,
    /// The durable task record this invocation was submitted as.
    ///
    /// Set by the admission step that created it. The worker adopts this
    /// record instead of creating a second one, so the task the model can see
    /// and the task that consumes capacity are the same task.
    pub(crate) submitted_task_id: Option<String>,
}

/// Synchronous child delivery boundary. The child retains the source event on
/// failure, so retries present the same commit id and digest to the actor.
pub(crate) struct RuntimeSubagentActivitySink {
    pub(crate) ingress: Arc<dyn RuntimeSubagentActivityIngress>,
}

#[cfg(test)]
#[derive(Debug, Default)]
struct TestSubagentActivityCollector {
    events: Mutex<Vec<SubagentActivityEvent>>,
}

#[cfg(test)]
impl RuntimeSubagentActivityIngress for TestSubagentActivityCollector {
    fn owner(&self) -> SubagentActivityOwner {
        SubagentActivityOwner::DetachedTask {
            task_id: SurfaceTaskId::try_new("headless-test-owner").expect("test task id"),
            task_revision: crate::runtime_surface::TaskRevision::try_new(1)
                .expect("test task revision"),
            authority_digest: crate::runtime_surface::Sha256Digest::new([0; 32]),
        }
    }

    fn commit_activity(&self, event: SubagentActivityEvent) -> io::Result<()> {
        if !event.verify_digest() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "child activity digest verification failed",
            ));
        }
        self.events
            .lock()
            .map_err(|_| io::Error::other("headless activity collector lock poisoned"))?
            .push(event);
        Ok(())
    }
}

#[cfg(test)]
pub(crate) fn test_activity_ingress() -> Arc<dyn RuntimeSubagentActivityIngress> {
    Arc::new(TestSubagentActivityCollector::default())
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

#[derive(Debug, Default)]
struct LegacySubagentActivitySink;

impl ChildAgentActivitySink for LegacySubagentActivitySink {
    fn publish(&self, event: SubagentActivityEvent) -> io::Result<()> {
        if event.verify_digest() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "child activity digest verification failed",
            ))
        }
    }
}

pub(crate) struct ChildEventActivityObserver {
    pub(crate) emitter: Arc<ChildAgentActivityEmitter>,
}

impl EventObserver for ChildEventActivityObserver {
    fn observe(&self, event: &EventEnvelope) -> io::Result<()> {
        let required_text = |field: &str| {
            event.payload[field]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "child event {:?} is missing string field '{field}'",
                            event.event_type
                        ),
                    )
                })
        };
        let activity = match event.event_type {
            EventType::TurnStarted => Some(crate::agent_child::ChildAgentActivity::TurnStarted {
                turn: event.payload["turn"].as_u64().unwrap_or_default() as u32,
            }),
            EventType::AssistantReasoningDelta | EventType::AssistantMessageDelta => {
                Some(crate::agent_child::ChildAgentActivity::Streaming)
            }
            EventType::ToolCallRequested => {
                Some(crate::agent_child::ChildAgentActivity::ToolStarted {
                    call_id: required_text("id")?,
                    name: required_text("name")?,
                    target: event.payload["target"].as_str().map(str::to_string),
                })
            }
            EventType::ToolCallCompleted => {
                Some(crate::agent_child::ChildAgentActivity::ToolCompleted {
                    call_id: required_text("id")?,
                    name: required_text("name")?,
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
        agent_controller: Option<Arc<AgentController>>,
        batch_id: String,
        batch_size: u32,
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
            agent_controller,
            batch_id,
            batch_size,
            submitted_task_id: None,
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

/// A child that was accepted and is waiting for an execution lease.
///
/// A queued child has no thread and no process: everything it needs to start
/// later lives here, and nothing about it exists in the operating system until
/// the scope grants it a lease.
struct PendingSubagentWorker {
    index: usize,
    invocation: Box<RuntimeSubagentInvocation>,
    task_id: String,
    tool_id: String,
    description: String,
}

/// What starting one child produced.
struct SpawnOutcome {
    /// The child never started; the caller reports this in place of a result.
    immediate: Option<RuntimeSubagentCallOutput>,
    /// The started event could not be delivered.
    event_error: Option<io::Error>,
}

pub(crate) struct RuntimeSubagentBatch {
    cancel: CancelToken,
    registry: TaskRegistry,
    limits: orca_core::subagent_config::SubagentLimits,
    /// The task that dispatched these children: their parent in the task tree.
    /// `None` when the root session dispatches, in which case the children
    /// hang off the root task.
    parent_task_id: Option<String>,
    /// The root task, used as the parent when the dispatcher is the root.
    root_task_id: Option<String>,
    workers: Vec<RuntimeSubagentWorker>,
    pending: Vec<PendingSubagentWorker>,
}

/// How long a waiting batch sleeps before looking again.
///
/// Wakeups come from the scope arbiter on every capacity change; this only
/// bounds how long a missed announcement, a panicking worker, or a cancelled
/// parent can go unnoticed. It is not a deadline and never cancels work.
const BATCH_WAIT_SLICE: std::time::Duration = std::time::Duration::from_millis(250);

fn enqueue_accepted_subagent(
    mut invocation: RuntimeSubagentInvocation,
    id: String,
) -> io::Result<()> {
    let registry = invocation.task_registry.clone();
    let limits = invocation.config.subagents.limits;
    let mut budget_guard = crate::child_budget_ledger::PendingChildBudget::new(
        invocation.request.budget_reservation.clone(),
    );
    invocation.submitted_task_id = Some(id.clone());
    invocation.request.mode = crate::subagent::SubagentMode::Sync;
    if invocation.request.resume_from.is_some() {
        invocation.agent_controller = None;
    }
    let worker_registry = registry.clone();
    let worker_id = id.clone();
    registry
        .scope_arbiter()
        .enqueue(registry.clone(), id, limits, move || {
            if let Err(error) = worker_registry.mark_subagent_execution_started(&worker_id) {
                let _ = worker_registry.fail(&worker_id, error);
                return;
            }
            budget_guard.start();
            let cancel = worker_registry
                .get(&worker_id)
                .map(|task| task.control.cancel.clone())
                .unwrap_or_default();
            let mut lifecycle = RuntimeSessionLifecycle::new(format!("subagent-{worker_id}"));
            let started = lifecycle.start_task(RuntimeTaskKind::Subagent).clone();
            let output = run_subagent_worker(invocation, lifecycle, started, cancel);
            if let Some(error) = output.event_error {
                let _ = worker_registry.fail(&worker_id, error);
            }
        })
}

/// Reattach durable, never-started launch intents to the process-local
/// dispatcher before the owner asks the model for more work.
///
/// A task whose execution-start bit was persisted is deliberately excluded:
/// after a crash its side effects are indeterminate and replay would violate
/// at-most-once execution. Policy is also revalidated at recovery time. Any
/// change fails closed instead of silently retaining revoked permissions or
/// expanding the original delegation.
#[allow(clippy::too_many_arguments)]
pub(crate) fn recover_queued_subagent_launches(
    config: &RunConfig,
    mcp_registry: &McpRegistry,
    hooks: &HookRunner,
    child_executor: ChildAgentExecutor<io::Sink>,
    activity_ingress: Option<Arc<dyn RuntimeSubagentActivityIngress>>,
    permission_handler: Option<Arc<dyn RuntimePermissionRequestHandler + Send + Sync>>,
    task_registry: &TaskRegistry,
    owner: Option<&str>,
    agent_controller: Option<Arc<AgentController>>,
) -> io::Result<usize> {
    let launches = task_registry
        .queued_subagent_launches(owner)
        .map_err(io::Error::other)?;
    let current_policy = DelegationSnapshot::from_config(config);
    let mut recovered = 0;
    for (task_id, intent) in launches {
        if !queued_policy_is_unchanged(&intent.request, &current_policy) {
            task_registry
                .fail(
                    &task_id,
                    "queued subagent policy changed before execution; submit it again under the current policy"
                        .into(),
                )
                .map_err(io::Error::other)?;
            continue;
        }
        let instructions = crate::instructions::load_for_cwd_or_default(&intent.cwd);
        let memory = crate::memory::load_for_cwd(&intent.cwd);
        let invocation = RuntimeSubagentInvocation::snapshot(
            intent.tool_request,
            intent.request,
            config,
            &intent.cwd,
            &instructions,
            &memory,
            mcp_registry,
            hooks,
            None,
            intent.child_depth,
            child_executor,
            activity_ingress.clone(),
            permission_handler.clone(),
            task_registry,
            owner,
            agent_controller.clone(),
            intent.batch_id,
            intent.batch_size,
        );
        enqueue_accepted_subagent(invocation, task_id)?;
        recovered += 1;
    }
    Ok(recovered)
}

fn queued_policy_is_unchanged(request: &SubagentRequest, current: &DelegationSnapshot) -> bool {
    request.delegation.as_ref() == Some(current)
}

impl RuntimeToolCallRuntime {
    pub(crate) fn start_subagent_batch(
        &self,
        cancel: &CancelToken,
        registry: &TaskRegistry,
        limits: orca_core::subagent_config::SubagentLimits,
        parent_task_id: Option<&str>,
        root_task_id: Option<&str>,
    ) -> RuntimeSubagentBatch {
        RuntimeSubagentBatch {
            cancel: cancel.clone(),
            registry: registry.clone(),
            limits: limits.normalized(),
            parent_task_id: parent_task_id.map(str::to_string),
            root_task_id: root_task_id.map(str::to_string),
            workers: Vec::new(),
            pending: Vec::new(),
        }
    }

    fn submit_background_subagent(
        &self,
        invocation: RuntimeSubagentInvocation,
    ) -> RuntimeSubagentExecution {
        let registry = invocation.task_registry.clone();
        let limits = invocation.config.subagents.limits;
        let tool_request = invocation.tool_request.clone();
        let description = invocation.request.description.clone();
        let launch_intent = crate::tasks::QueuedSubagentLaunchIntent {
            schema_version: crate::tasks::QueuedSubagentLaunchIntent::SCHEMA_VERSION,
            tool_request: tool_request.clone(),
            request: invocation.request.clone(),
            cwd: invocation.cwd.clone(),
            child_depth: invocation.child_depth,
            batch_id: invocation.batch_id.clone(),
            batch_size: invocation.batch_size,
        };
        let submission = registry
            .execution_scope(&limits)
            .submit_subagent_with_intent(
                description.clone(),
                serialized_subagent_type(&invocation.request.subagent_type),
                invocation.root_task_id.clone(),
                now_unix_ms(),
                Some(launch_intent),
            );
        let result = match submission {
            Err(error) => ToolResult::failed_before_start(&tool_request, error.message(), None),
            Ok(submission) if submission.cancelled => {
                ToolResult::cancelled_before_start(&tool_request, "parent cancelled")
            }
            Ok(submission) => {
                let id = submission.task_id;
                registry.mark_subagent_result_pending(&id);
                if let Some(reservation) = &invocation.request.budget_reservation {
                    if let Err(error) = reservation.bind_task(&id).and_then(|_| {
                        registry
                            .bind_child_budget(&id, reservation.clone())
                            .map_err(io::Error::other)
                    }) {
                        let _ = registry.fail(&id, error.to_string());
                        return RuntimeSubagentExecution {
                            output: failed_before_start(invocation, error.to_string()),
                            event_error: None,
                        };
                    }
                }
                if let Some(ms) = invocation.request.deadline_ms {
                    registry.set_deadline_at(
                        &id,
                        now_unix_ms().saturating_add(ms.min(i64::MAX as u64) as i64),
                        "caller deadline",
                    );
                }
                let status = if submission.admission == Admission::Started {
                    "running"
                } else {
                    "queued"
                };
                let payload = serde_json::json!({"task_id": id, "agent_id": id, "accepted": true, "status": status});
                let scheduled = enqueue_accepted_subagent(invocation, id.clone());
                match scheduled {
                    Ok(()) => ToolResult::running(&tool_request, payload.to_string(), false),
                    Err(error) => {
                        let _ = registry.fail(&id, error.to_string());
                        ToolResult::failed_before_start(&tool_request, error.to_string(), None)
                    }
                }
            }
        };
        RuntimeSubagentExecution {
            output: RuntimeSubagentCallOutput {
                tool_request,
                description,
                result,
                task: None,
                status: RunStatus::Success,
                event_output: None,
                event_error: None,
                cost_tracker: CostTracker::new(None),
                child_budget_usage: None,
            },
            event_error: None,
        }
    }

    pub(crate) fn execute_subagent(
        &self,
        invocation: RuntimeSubagentInvocation,
        cancel: &CancelToken,
        publish_started: impl FnOnce(&RuntimeTaskLifecycle) -> io::Result<()>,
    ) -> RuntimeSubagentExecution {
        if invocation.request.mode == crate::subagent::SubagentMode::Async {
            if cancel.is_cancelled() {
                return RuntimeSubagentExecution {
                    output: cancelled_before_start(invocation),
                    event_error: None,
                };
            }
            return self.submit_background_subagent(invocation);
        }
        let mut publish_started = Some(publish_started);
        let registry = invocation.task_registry.clone();
        let limits = invocation.config.subagents.limits;
        let root_task_id = invocation.root_task_id.clone();
        let mut batch =
            self.start_subagent_batch(cancel, &registry, limits, None, root_task_id.as_deref());
        let admission = batch.admit(0, invocation, |task| match publish_started.take() {
            Some(publish) => publish(task),
            None => Ok(()),
        });
        let mut output = admission.immediate.map(|(_, output)| output);
        let mut event_error = admission.event_error;
        let (completed, late_error) =
            batch.finish(&mut |_, task, _, _| match publish_started.take() {
                Some(publish) => publish(task),
                None => Ok(()),
            });
        if event_error.is_none() {
            event_error = late_error;
        }
        if let Some((_, completed)) = completed.into_iter().next() {
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
    /// Accepts one child, then starts it or leaves it queued.
    ///
    /// The record is created before any capacity is reserved, so an accepted
    /// child is observable and steerable while it waits, and a restart can
    /// rebuild the queue from it. A capacity refusal creates nothing: it is a
    /// boundary, not a queue position.
    pub(crate) fn admit(
        &mut self,
        index: usize,
        mut invocation: RuntimeSubagentInvocation,
        publish_started: impl FnOnce(&RuntimeTaskLifecycle) -> io::Result<()>,
    ) -> RuntimeSubagentAdmission {
        if self.cancel.is_cancelled() {
            return RuntimeSubagentAdmission {
                immediate: Some((index, cancelled_before_start(invocation))),
                event_error: None,
            };
        }
        if let Err(error) = crate::subagent::freeze_agent_request(
            &invocation.config,
            &invocation.cwd,
            &invocation.mcp_registry,
            &mut invocation.request,
        ) {
            return RuntimeSubagentAdmission {
                immediate: Some((index, failed_before_start(invocation, error))),
                event_error: None,
            };
        }
        let scope = self.registry.execution_scope(&self.limits);
        let parent = self
            .parent_task_id
            .clone()
            .or_else(|| self.root_task_id.clone());
        let launch_intent = crate::tasks::QueuedSubagentLaunchIntent {
            schema_version: crate::tasks::QueuedSubagentLaunchIntent::SCHEMA_VERSION,
            tool_request: invocation.tool_request.clone(),
            request: invocation.request.clone(),
            cwd: invocation.cwd.clone(),
            child_depth: invocation.child_depth,
            batch_id: invocation.batch_id.clone(),
            batch_size: invocation.batch_size,
        };
        let submission = match scope.submit_subagent_with_intent(
            invocation.request.description.clone(),
            serialized_subagent_type(&invocation.request.subagent_type),
            parent,
            now_unix_ms(),
            Some(launch_intent),
        ) {
            Ok(submission) => submission,
            Err(refusal) => {
                return RuntimeSubagentAdmission {
                    immediate: Some((index, failed_before_start(invocation, refusal.message()))),
                    event_error: None,
                };
            }
        };
        let task_id = submission.task_id;
        // The deadline is armed at submission, so a child that expires while
        // it is queued is stopped instead of being started late.
        arm_subagent_deadline(&self.registry, &task_id, invocation.request.deadline_ms);
        let tool_id = invocation.tool_request.id.clone();
        let description = invocation.request.description.clone();
        if submission.cancelled {
            // The tree was already cancelled, so this child can never run; the
            // submission already settled its record.
            return RuntimeSubagentAdmission {
                immediate: Some((
                    index,
                    cancelled_task_output(invocation, &self.registry, &task_id),
                )),
                event_error: None,
            };
        }
        let mut once = Some(publish_started);
        let mut publish =
            |_index: usize, task: &RuntimeTaskLifecycle, _: &str, _: &str| match once.take() {
                Some(publish) => publish(task),
                None => Ok(()),
            };
        match submission.admission {
            Admission::Started => {
                let outcome = self.spawn(
                    index,
                    invocation,
                    task_id,
                    tool_id,
                    description,
                    &mut publish,
                );
                RuntimeSubagentAdmission {
                    immediate: outcome.immediate.map(|output| (index, output)),
                    event_error: outcome.event_error,
                }
            }
            Admission::Queued => {
                self.pending.push(PendingSubagentWorker {
                    index,
                    invocation: Box::new(invocation),
                    task_id,
                    tool_id,
                    description,
                });
                RuntimeSubagentAdmission {
                    immediate: None,
                    event_error: None,
                }
            }
        }
    }

    /// Starts one child that holds a lease.
    ///
    /// A child that could not be started is reported through
    /// [`SpawnOutcome::immediate`]; a started child is joined later, from
    /// [`RuntimeSubagentBatch::finish`].
    fn spawn(
        &mut self,
        index: usize,
        mut invocation: RuntimeSubagentInvocation,
        task_id: String,
        tool_id: String,
        description: String,
        publish_started: &mut dyn FnMut(usize, &RuntimeTaskLifecycle, &str, &str) -> io::Result<()>,
    ) -> SpawnOutcome {
        if let Err(error) = self.registry.mark_subagent_execution_started(&task_id) {
            let _ = self.registry.fail(&task_id, error.clone());
            return SpawnOutcome {
                immediate: Some(failed_before_start(invocation, error)),
                event_error: None,
            };
        }
        invocation.request.mode = crate::subagent::SubagentMode::Sync;
        let mut lifecycle = RuntimeSessionLifecycle::new(format!("subagent-{tool_id}"));
        let started_task = lifecycle.start_task(RuntimeTaskKind::Subagent).clone();
        if let Err(error) = publish_started(index, &started_task, &tool_id, &description) {
            let message = "subagent dispatch stopped because its started event could not be \
                           delivered";
            let _ = self.registry.fail(&task_id, message.to_string());
            return SpawnOutcome {
                immediate: Some(RuntimeSubagentCallOutput {
                    result: ToolResult::failed_before_start(
                        &invocation.tool_request,
                        message,
                        None,
                    ),
                    tool_request: invocation.tool_request.clone(),
                    description: invocation.request.description.clone(),
                    task: Some(started_task.with_status(RuntimeTaskStatus::Failed)),
                    status: RunStatus::Failed,
                    event_output: None,
                    event_error: Some(message.to_string()),
                    cost_tracker: CostTracker::new(None),
                    child_budget_usage: None,
                }),
                event_error: Some(error),
            };
        }

        let tool_request = invocation.tool_request.clone();
        let description = invocation.request.description.clone();
        let panic_request = tool_request.clone();
        let panic_description = description.clone();
        let panic_task = started_task.clone();
        let worker_cancel = if invocation.agent_controller.is_some() {
            let worker_cancel = CancelToken::new();
            let parent_cancel = self.cancel.clone();
            let worker_cancel_bridge = worker_cancel.clone();
            let watcher = thread::Builder::new()
                .name(format!("orca-subagent-parent-cancel-{}", tool_request.id))
                .spawn(move || {
                    while !worker_cancel_bridge.is_cancelled() {
                        if parent_cancel.is_cancelled() {
                            worker_cancel_bridge.cancel();
                            break;
                        }
                        thread::sleep(std::time::Duration::from_millis(10));
                    }
                });
            if let Err(error) = watcher {
                let message = format!("failed to watch parent cancellation: {error}");
                let _ = self.registry.fail(&task_id, message.clone());
                return SpawnOutcome {
                    immediate: Some(RuntimeSubagentCallOutput {
                        result: ToolResult::failed_before_start(&panic_request, &message, None),
                        tool_request: panic_request,
                        description: panic_description,
                        task: Some(panic_task.with_status(RuntimeTaskStatus::Failed)),
                        status: RunStatus::Failed,
                        event_output: None,
                        event_error: Some(message),
                        cost_tracker: CostTracker::new(None),
                        child_budget_usage: None,
                    }),
                    event_error: None,
                };
            }
            worker_cancel
        } else {
            self.cancel.clone()
        };
        let worker_cancel_for_thread = worker_cancel.clone();
        let watcher_cancel = worker_cancel.clone();
        let worker_cleanup = invocation
            .agent_controller
            .as_ref()
            .map(|_| worker_cancel.clone());
        // The worker adopts the record this batch submitted instead of
        // creating its own, so capacity accounting and the model's view of the
        // task describe the same task.
        invocation.submitted_task_id = Some(task_id.clone());
        let join = match thread::Builder::new()
            .name(format!("orca-subagent-{}", tool_request.id))
            .spawn(move || {
                let output = run_subagent_worker(
                    invocation,
                    lifecycle,
                    started_task,
                    worker_cancel_for_thread.clone(),
                );
                if let Some(worker_cleanup) = worker_cleanup {
                    worker_cleanup.cancel();
                }
                output
            }) {
            Ok(join) => join,
            Err(error) => {
                watcher_cancel.cancel();
                let message = format!("failed to start subagent worker: {error}");
                let _ = self.registry.fail(&task_id, message.clone());
                return SpawnOutcome {
                    immediate: Some(RuntimeSubagentCallOutput {
                        tool_request: panic_request.clone(),
                        description: panic_description,
                        task: Some(panic_task.with_status(RuntimeTaskStatus::Failed)),
                        status: RunStatus::Failed,
                        result: ToolResult::failed_before_start(&panic_request, &message, None),
                        event_output: None,
                        event_error: Some(message),
                        cost_tracker: CostTracker::new(None),
                        child_budget_usage: None,
                    }),
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
        SpawnOutcome {
            immediate: None,
            event_error: None,
        }
    }

    /// Waits for every child, admitting queued ones as leases free.
    ///
    /// A parent that blocks here gives up its own execution lease first, and
    /// takes one again before it continues: waiting is not running, and coming
    /// back from a wait is not a way around the capacity bound. That is what
    /// lets a tree make progress when every running task is a parent waiting
    /// for a descendant.
    pub(crate) fn finish(
        &mut self,
        publish_started: &mut dyn FnMut(usize, &RuntimeTaskLifecycle, &str, &str) -> io::Result<()>,
    ) -> (Vec<(usize, RuntimeSubagentCallOutput)>, Option<io::Error>) {
        // The scope borrows a registry handle of its own so the wait loop can
        // still take `&mut self` to start and reap children.
        let registry = self.registry.clone();
        let scope = registry.execution_scope(&self.limits);
        let mut completed: Vec<(usize, RuntimeSubagentCallOutput)> = Vec::new();
        let mut event_error = None;
        let waiting_parent = self
            .parent_task_id
            .clone()
            .filter(|_| !self.pending.is_empty() || !self.workers.is_empty());
        if let Some(parent) = waiting_parent.as_ref() {
            self.registry
                .requeue_for_resume(parent, crate::task_view::WaitReason::Dependency);
        }
        loop {
            self.collect_finished(&mut completed);
            let mut started = Vec::new();
            self.start_pending(&scope, publish_started, &mut started, &mut event_error);
            completed.extend(started);
            if self.workers.is_empty() && self.pending.is_empty() {
                break;
            }
            if self.cancel.is_cancelled() {
                break;
            }
            let _ = scope.wait_for_capacity(Some(&self.cancel), BATCH_WAIT_SLICE);
        }
        self.collect_finished(&mut completed);
        for worker in std::mem::take(&mut self.workers) {
            completed.push((worker.index, join_worker(worker)));
        }
        // Every accepted child has a result, including one that never got a
        // lease: the batch reports it as not started and closes the record, so
        // it neither looks runnable later nor leaves a queue position behind.
        for item in std::mem::take(&mut self.pending) {
            if let Some(output) = self.abandoned_output(&item) {
                completed.push((item.index, output));
                continue;
            }
            let _ = self.registry.stop(
                &item.task_id,
                "the batch ended before this task could start".to_string(),
            );
            completed.push((item.index, not_started_output(&item)));
        }
        if let Some(parent) = waiting_parent.as_ref() {
            self.wait_for_parent_lease(&scope, parent);
        }
        (completed, event_error)
    }

    /// Starts whatever pending children the scope will admit now.
    ///
    /// A child that was stopped, cancelled, or whose deadline passed while it
    /// waited is reported instead of started: a queued task never begins after
    /// the reason it was waiting has expired.
    fn start_pending(
        &mut self,
        scope: &crate::execution_scope::ExecutionScope<'_>,
        publish_started: &mut dyn FnMut(usize, &RuntimeTaskLifecycle, &str, &str) -> io::Result<()>,
        started: &mut Vec<(usize, RuntimeSubagentCallOutput)>,
        event_error: &mut Option<io::Error>,
    ) {
        let mut pending = Vec::new();
        for item in std::mem::take(&mut self.pending) {
            if let Some(output) = self.abandoned_output(&item) {
                started.push((item.index, output));
                continue;
            }
            match scope.acquire(&item.task_id, now_unix_ms()) {
                Admission::Started => {
                    let PendingSubagentWorker {
                        index,
                        invocation,
                        task_id,
                        tool_id,
                        description,
                    } = item;
                    let outcome = self.spawn(
                        index,
                        *invocation,
                        task_id,
                        tool_id,
                        description,
                        publish_started,
                    );
                    if event_error.is_none() {
                        *event_error = outcome.event_error;
                    }
                    if let Some(output) = outcome.immediate {
                        started.push((index, output));
                    }
                }
                Admission::Queued => pending.push(item),
            }
        }
        self.pending = pending;
    }

    /// The result for a queued child that must not start.
    fn abandoned_output(&self, item: &PendingSubagentWorker) -> Option<RuntimeSubagentCallOutput> {
        let record = self.registry.get(&item.task_id)?;
        let cancelled = record.control.cancel.is_cancelled()
            || matches!(record.status, orca_core::task_types::TaskStatus::Cancelled);
        let stopped = matches!(
            record.status,
            orca_core::task_types::TaskStatus::Stopped
                | orca_core::task_types::TaskStatus::Completed
                | orca_core::task_types::TaskStatus::Failed
        );
        if !cancelled && !stopped {
            return None;
        }
        let reason = record
            .error
            .clone()
            .or(record.result.clone())
            .unwrap_or_else(|| "the task was closed before it started".to_string());
        let (status, result) = if cancelled {
            (
                RunStatus::Cancelled,
                ToolResult::cancelled_before_start(&item.invocation.tool_request, &reason),
            )
        } else {
            (
                RunStatus::Failed,
                ToolResult::failed_before_start(&item.invocation.tool_request, &reason, None),
            )
        };
        Some(RuntimeSubagentCallOutput {
            tool_request: item.invocation.tool_request.clone(),
            description: item.invocation.request.description.clone(),
            task: None,
            status,
            result,
            event_output: None,
            event_error: None,
            cost_tracker: CostTracker::new(None),
            child_budget_usage: None,
        })
    }

    fn collect_finished(&mut self, completed: &mut Vec<(usize, RuntimeSubagentCallOutput)>) {
        let mut remaining = Vec::new();
        for worker in std::mem::take(&mut self.workers) {
            if worker.join.is_finished() {
                completed.push((worker.index, join_worker(worker)));
            } else {
                remaining.push(worker);
            }
        }
        self.workers = remaining;
    }

    /// Takes a lease again before the parent continues past the wait.
    ///
    /// The parent is `queued` while it waits, so nothing else has to infer
    /// that it is awake; it becomes `running` only when the scope admits it.
    fn wait_for_parent_lease(
        &self,
        scope: &crate::execution_scope::ExecutionScope<'_>,
        parent: &str,
    ) {
        loop {
            if self.cancel.is_cancelled() {
                return;
            }
            match self.registry.get(parent) {
                // The parent was stopped while it waited: the caller's own
                // loop reports the cancellation instead of resuming.
                None => return,
                Some(record)
                    if record.control.cancel.is_cancelled()
                        || matches!(
                            record.status,
                            orca_core::task_types::TaskStatus::Stopped
                                | orca_core::task_types::TaskStatus::Completed
                                | orca_core::task_types::TaskStatus::Failed
                                | orca_core::task_types::TaskStatus::Cancelled
                        ) =>
                {
                    return;
                }
                Some(_) => {}
            }
            if scope.acquire(parent, now_unix_ms()) == Admission::Started {
                return;
            }
            let _ = scope.wait_for_capacity(Some(&self.cancel), BATCH_WAIT_SLICE);
        }
    }
}

impl Drop for RuntimeSubagentBatch {
    /// Nothing queued by this batch is left behind when it goes away.
    ///
    /// A child that was accepted but never started is stopped with a reason,
    /// so it neither holds a queue position forever nor looks runnable after
    /// the turn that submitted it has ended.
    fn drop(&mut self) {
        for item in std::mem::take(&mut self.pending) {
            let _ = self.registry.stop(
                &item.task_id,
                "the submitting turn ended before this task could start".to_string(),
            );
        }
    }
}

fn join_worker(worker: RuntimeSubagentWorker) -> RuntimeSubagentCallOutput {
    match worker.join.join() {
        Ok(output) => output,
        Err(payload) => {
            let error = format!(
                "Subagent worker panicked after execution started: {}. Inspect external state before retrying.",
                panic_payload_message(payload)
            );
            RuntimeSubagentCallOutput {
                result: ToolResult::indeterminate_after_start(&worker.tool_request, &error),
                tool_request: worker.tool_request,
                description: worker.description,
                task: Some(worker.started_task.with_status(RuntimeTaskStatus::Failed)),
                status: RunStatus::Failed,
                event_output: None,
                event_error: Some(error),
                cost_tracker: CostTracker::new(None),
                child_budget_usage: None,
            }
        }
    }
}

/// The result for a child that was accepted and never started.
fn not_started_output(item: &PendingSubagentWorker) -> RuntimeSubagentCallOutput {
    let message = "the task was accepted but did not start before the submitting turn ended";
    RuntimeSubagentCallOutput {
        result: ToolResult::cancelled_before_start(&item.invocation.tool_request, message),
        tool_request: item.invocation.tool_request.clone(),
        description: item.invocation.request.description.clone(),
        task: None,
        status: RunStatus::Cancelled,
        event_output: None,
        event_error: None,
        cost_tracker: CostTracker::new(None),
        child_budget_usage: None,
    }
}

fn cancelled_task_output(
    invocation: RuntimeSubagentInvocation,
    registry: &TaskRegistry,
    task_id: &str,
) -> RuntimeSubagentCallOutput {
    let message = registry
        .get(task_id)
        .and_then(|record| record.error.or(record.result))
        .unwrap_or_else(|| {
            "the task tree was already cancelled when this task was submitted".to_string()
        });
    RuntimeSubagentCallOutput {
        result: ToolResult::cancelled_before_start(&invocation.tool_request, &message),
        tool_request: invocation.tool_request,
        description: invocation.request.description,
        task: None,
        status: RunStatus::Cancelled,
        event_output: None,
        event_error: None,
        cost_tracker: CostTracker::new(None),
        child_budget_usage: None,
    }
}

fn run_threaded_agent_worker(
    invocation: RuntimeSubagentInvocation,
    _lifecycle: RuntimeSessionLifecycle,
    started_task: RuntimeTaskLifecycle,
    cancel: CancelToken,
) -> RuntimeSubagentCallOutput {
    let RuntimeSubagentInvocation {
        tool_request,
        request,
        config,
        child_depth: _,
        activity_ingress,
        permission_handler,
        agent_controller,
        batch_id,
        batch_size,
        task_registry,
        root_task_id,
        submitted_task_id,
        ..
    } = invocation;
    let controller = agent_controller.expect("threaded agent branch requires controller");
    let description = request.description.clone();
    let mode = match request.mode {
        crate::subagent::SubagentMode::Sync => AgentRunMode::Sync,
        crate::subagent::SubagentMode::Async => AgentRunMode::Async,
    };
    let (surface_activity, permission_handler, registry_task_id, launch_cancel) = if mode
        == AgentRunMode::Sync
        && activity_ingress.is_some()
        && request.isolation == SubagentIsolation::None
    {
        if request.resume_from.is_some() {
            let message = "hosted_sync_resume_unsupported: synchronous hosted subagents cannot resume a continuation";
            // The task was already accepted when this invocation was
            // submitted, so a rejection here has to settle it: a record left
            // behind would keep a queue position for work that never ran.
            if let Some(task_id) = submitted_task_id.as_deref() {
                let _ = task_registry.fail(task_id, message.to_string());
            }
            return RuntimeSubagentCallOutput {
                result: ToolResult::failed_before_start(&tool_request, message, None),
                tool_request,
                description,
                task: Some(started_task.with_status(RuntimeTaskStatus::Failed)),
                status: RunStatus::Failed,
                event_output: None,
                event_error: Some(message.to_string()),
                cost_tracker: CostTracker::new(config.model.as_deref()),
                child_budget_usage: None,
            };
        }
        // The admission step already recorded this task; adopting its id is
        // what keeps one child from becoming two records. Only a caller that
        // bypassed admission creates its own record here.
        let registry_task_id = match submitted_task_id {
            Some(task_id) => task_id,
            None => {
                let registry_task = task_registry.create_subagent_with_parent(
                    description.clone(),
                    serialized_subagent_type(&request.subagent_type),
                    root_task_id.clone(),
                );
                arm_subagent_deadline(&task_registry, &registry_task.id, request.deadline_ms);
                registry_task.id
            }
        };
        if let Err(error) = task_registry.mark_running(&registry_task_id) {
            let message = format!("failed to mark threaded subagent task running: {error}");
            let _ = task_registry.fail(&registry_task_id, message.clone());
            return RuntimeSubagentCallOutput {
                result: ToolResult::failed_before_start(&tool_request, &message, None),
                tool_request,
                description,
                task: Some(started_task.with_status(RuntimeTaskStatus::Failed)),
                status: RunStatus::Failed,
                event_output: None,
                event_error: Some(message),
                cost_tracker: CostTracker::new(config.model.as_deref()),
                child_budget_usage: None,
            };
        }
        // Execute against the canonical task's own cancel token. `task_stop`
        // flips this exact token, so there is no polling race between marking
        // the task running and installing a cancellation watcher.
        let task_cancel = task_registry
            .get(&registry_task_id)
            .expect("threaded subagent task exists after mark_running")
            .control
            .cancel;
        // Parent cancellation remains a separate one-way edge. Stopping this
        // child must never cancel its parent or a sibling.
        let registry_for_cancel = task_registry.clone();
        let parent_cancel = cancel.clone();
        let watcher_task_cancel = task_cancel.clone();
        let task_id_for_cancel = registry_task_id.clone();
        let watcher = std::thread::Builder::new()
            .name(format!("orca-agent-cancel-{}", task_id_for_cancel))
            .spawn(move || {
                loop {
                    if parent_cancel.is_cancelled() {
                        watcher_task_cancel.cancel();
                        break;
                    }
                    let terminal =
                        registry_for_cancel
                            .get(&task_id_for_cancel)
                            .is_none_or(|task| {
                                !task.status.is_active() && !task.status.requires_attention()
                            });
                    if terminal {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            });
        if let Err(error) = watcher {
            let message = format!("failed to watch child cancellation: {error}");
            let _ = task_registry.fail(&registry_task_id, message.clone());
            return RuntimeSubagentCallOutput {
                result: ToolResult::failed_before_start(&tool_request, &message, None),
                tool_request,
                description,
                task: Some(started_task.with_status(RuntimeTaskStatus::Failed)),
                status: RunStatus::Failed,
                event_output: None,
                event_error: Some(message),
                cost_tracker: CostTracker::new(config.model.as_deref()),
                child_budget_usage: None,
            };
        }
        match threaded_sync_surface_activity(
            &registry_task_id,
            &registry_task_id,
            crate::agent_continuation::AgentAttemptId::new(),
            &description,
            &batch_id,
            batch_size,
            activity_ingress.as_ref().cloned(),
        ) {
            Ok(activity) => {
                let permission_handler = permission_handler.map(|parent| {
                    Arc::new(ChildPermissionHandler::new(
                        parent,
                        activity.permission_identity,
                    )) as Arc<dyn RuntimePermissionRequestHandler + Send + Sync>
                });
                (
                    Some(activity.surface_activity),
                    permission_handler,
                    Some(registry_task_id),
                    task_cancel,
                )
            }
            Err(error) => {
                let message = error.to_string();
                let _ = task_registry.fail(&registry_task_id, message.clone());
                return RuntimeSubagentCallOutput {
                    result: ToolResult::failed_before_start(&tool_request, &message, None),
                    tool_request,
                    description,
                    task: Some(started_task.with_status(RuntimeTaskStatus::Failed)),
                    status: RunStatus::Failed,
                    event_output: None,
                    event_error: Some(message),
                    cost_tracker: CostTracker::new(config.model.as_deref()),
                    child_budget_usage: None,
                };
            }
        }
    } else {
        // Without a surface ingress the child still is a task in this tree:
        // admission recorded it, so it has to be marked running here. A record
        // left `queued` while the child actually executes would keep a queue
        // position it is not using and hide the lease it does hold.
        let registry_task_id = submitted_task_id.map(|task_id| {
            let _ = task_registry.mark_running(&task_id);
            task_id
        });
        (None, permission_handler, registry_task_id, cancel.clone())
    };
    let launch_surface_activity = surface_activity.clone();
    let registry_agent_id = registry_task_id
        .clone()
        .unwrap_or_else(|| tool_request.id.clone());
    let launch = controller.launch(AgentLaunchRequest {
        agent_id: registry_agent_id,
        task_registry: task_registry.clone(),
        description: description.clone(),
        prompt: request.prompt,
        model: request.model,
        subagent_type: request.subagent_type,
        mode,
        config: config.clone(),
        cancel: launch_cancel,
        // Subagents never inherit an interactive approval handler. The parent's
        // `RuntimeApprovalHandler` exists only as a non-'static borrow scoped to
        // the parent turn, so it cannot cross this worker boundary; the legacy
        // sync child path is identical. A child resolves per-tool approvals from
        // its own RunConfig (RuntimeConfigApprovalHandler), while user-facing
        // escalations flow through the scoped permission_handler below.
        approval_handler: None,
        permission_handler,
        surface_activity,
        batch_id: batch_id.clone(),
        batch_size,
    });

    match launch {
        Ok(launch) => {
            if let Some(id) = registry_task_id.as_deref() {
                let _ = task_registry.bind_subagent_thread(id, &launch.thread_id);
            }
            let mut status = launch.status;
            let mut output = if launch.running {
                let agent_id = registry_task_id
                    .as_deref()
                    .unwrap_or(tool_request.id.as_str());
                serde_json::json!({
                    "agent_id": agent_id,
                    "thread_id": launch.thread_id,
                    "status": "running",
                })
                .to_string()
            } else {
                launch
                    .final_message
                    .clone()
                    .unwrap_or_else(|| format!("agent {} completed", tool_request.id))
            };
            let schema_error = if status == RunStatus::Success {
                match validate_subagent_output_schema(
                    &description,
                    request.schema.as_ref(),
                    &output,
                ) {
                    Ok(normalized) => {
                        output = normalized;
                        None
                    }
                    Err(error) => Some(error),
                }
            } else {
                None
            };
            if schema_error.is_some() {
                status = RunStatus::Failed;
            }
            let mut result = if status == RunStatus::Success {
                ToolResult::completed(&tool_request, output.clone(), false)
            } else if status == RunStatus::Cancelled {
                ToolResult::cancelled(
                    &tool_request,
                    launch
                        .error
                        .clone()
                        .unwrap_or_else(|| "subagent cancelled".to_string()),
                    None,
                )
            } else {
                ToolResult::failed_after_start(
                    &tool_request,
                    schema_error
                        .or_else(|| launch.error.clone())
                        .unwrap_or_else(|| status.as_str().to_string()),
                    None,
                )
            };
            let mut cost_tracker = CostTracker::new(config.model.as_deref());
            cost_tracker.merge_totals(launch.usage);
            let usage = launch.usage;
            let registry_result = if launch.running {
                Ok(())
            } else {
                match (status, registry_task_id.as_deref()) {
                    (RunStatus::Success, Some(task_id)) => task_registry.complete_with_usage(
                        task_id,
                        output.clone(),
                        Some(orca_core::cost_types::UsageTotals {
                            input_tokens: usage.input_tokens,
                            output_tokens: usage.output_tokens,
                            cache_tokens: usage.cache_tokens,
                            estimated_cost_usd: usage.estimated_cost_usd,
                        }),
                    ),
                    (RunStatus::Cancelled, Some(task_id)) => task_registry.stop_with_usage(
                        task_id,
                        launch
                            .error
                            .clone()
                            .unwrap_or_else(|| "subagent cancelled".to_string()),
                        Some(orca_core::cost_types::UsageTotals {
                            input_tokens: usage.input_tokens,
                            output_tokens: usage.output_tokens,
                            cache_tokens: usage.cache_tokens,
                            estimated_cost_usd: usage.estimated_cost_usd,
                        }),
                    ),
                    (_, Some(task_id)) => task_registry.fail_with_usage(
                        task_id,
                        launch
                            .error
                            .clone()
                            .unwrap_or_else(|| status.as_str().to_string()),
                        Some(orca_core::cost_types::UsageTotals {
                            input_tokens: usage.input_tokens,
                            output_tokens: usage.output_tokens,
                            cache_tokens: usage.cache_tokens,
                            estimated_cost_usd: usage.estimated_cost_usd,
                        }),
                    ),
                    (_, None) => Ok(()),
                }
            };
            let mut event_error = launch.error;
            if let Err(error) = registry_result {
                event_error = Some(match event_error {
                    Some(existing) => {
                        format!("{existing}; task registry settlement failed: {error}")
                    }
                    None => format!("task registry settlement failed: {error}"),
                });
            }
            if let Some(activity) = launch_surface_activity.as_ref() {
                if let Err(surface_error) = activity.publish_terminal(
                    status,
                    (status == RunStatus::Success).then_some(output.as_str()),
                    event_error.as_deref(),
                    usage,
                ) {
                    let message = format!(
                        "surface terminal commit failed: {surface_error}; Inspect external state before retrying"
                    );
                    status = RunStatus::Failed;
                    event_error = Some(message.clone());
                    result = ToolResult::indeterminate_after_start(&tool_request, message);
                }
            }
            let task_status = if launch.running {
                RuntimeTaskStatus::Running
            } else {
                match status {
                    RunStatus::Success => RuntimeTaskStatus::Succeeded,
                    RunStatus::Cancelled => RuntimeTaskStatus::Cancelled,
                    RunStatus::Failed
                    | RunStatus::ApprovalRequired
                    | RunStatus::VerificationFailed => RuntimeTaskStatus::Failed,
                }
            };
            let task = Some(started_task.with_status(task_status));
            RuntimeSubagentCallOutput {
                tool_request,
                description,
                task,
                status,
                result,
                event_output: (!launch.running).then_some(output),
                event_error,
                cost_tracker,
                child_budget_usage: None,
            }
        }
        Err(error) => {
            let message = error.to_string();
            if let Some(task_id) = registry_task_id.as_deref() {
                let _ = task_registry.fail(task_id, message.clone());
            }
            RuntimeSubagentCallOutput {
                result: ToolResult::failed_before_start(&tool_request, &message, None),
                tool_request,
                description,
                task: Some(started_task.with_status(RuntimeTaskStatus::Failed)),
                status: RunStatus::Failed,
                event_output: None,
                event_error: Some(message),
                cost_tracker: CostTracker::new(config.model.as_deref()),
                child_budget_usage: None,
            }
        }
    }
}

struct ThreadedSyncSurfaceActivity {
    surface_activity: AgentSurfaceActivity,
    permission_identity: ChildPermissionIdentity,
}

fn threaded_sync_surface_activity(
    task_id: &str,
    subagent_id: &str,
    attempt_id: crate::agent_continuation::AgentAttemptId,
    description: &str,
    batch_id: &str,
    batch_size: u32,
    activity_ingress: Option<Arc<dyn RuntimeSubagentActivityIngress>>,
) -> io::Result<ThreadedSyncSurfaceActivity> {
    let Some(activity_ingress) = activity_ingress else {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "threaded synchronous subagent requires actor-owned activity ingress",
        ));
    };
    let task_id = SurfaceTaskId::try_new(task_id.to_string()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "threaded synchronous subagent has an invalid surface task id",
        )
    })?;
    let subagent_id = SurfaceSubagentId::try_new(subagent_id.to_string()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "threaded synchronous subagent has an invalid surface subagent id",
        )
    })?;
    let turn_id = TurnId::new();
    let emitter = Arc::new(ChildAgentActivityEmitter::new(
        SubagentActivityIdentity {
            task_id: task_id.clone(),
            subagent_id: subagent_id.clone(),
            attempt_id,
            turn_id: turn_id.clone(),
            owner: activity_ingress.owner(),
        },
        Arc::new(RuntimeSubagentActivitySink {
            ingress: activity_ingress,
        }),
    ));
    Ok(ThreadedSyncSurfaceActivity {
        permission_identity: ChildPermissionIdentity::new(
            task_id,
            subagent_id,
            turn_id,
            emitter.revision_source(),
        ),
        surface_activity: AgentSurfaceActivity {
            emitter,
            description: description.to_string(),
            batch_id: batch_id.to_string(),
            batch_size,
        },
    })
}

fn run_subagent_worker(
    invocation: RuntimeSubagentInvocation,
    lifecycle: RuntimeSessionLifecycle,
    started_task: RuntimeTaskLifecycle,
    cancel: CancelToken,
) -> RuntimeSubagentCallOutput {
    let resumes_custom = invocation
        .request
        .resume_from
        .as_deref()
        .is_some_and(|selector| {
            ChildAgentCoordinator::new(invocation.task_registry.clone())
                .and_then(|coordinator| coordinator.prepared(selector))
                .is_ok_and(|source| source.compatibility.frozen_agent.is_some())
        });
    if invocation.agent_controller.is_some()
        && invocation.request.isolation == SubagentIsolation::None
        && !matches!(invocation.request.subagent_type, SubagentType::Custom(_))
        && !resumes_custom
    {
        return run_threaded_agent_worker(invocation, lifecycle, started_task, cancel);
    }
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
        agent_controller: _,
        batch_id: _,
        batch_size: _,
        submitted_task_id,
    } = invocation;
    let SubagentRequest {
        budget_reservation,
        description,
        prompt,
        subagent_type: requested_subagent_type,
        model: requested_model,
        mode: _,
        isolation: requested_isolation,
        schema,
        resume_from,
        deadline_ms,
        delegation,
        frozen_agent,
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
    // Adopt the record created at admission when there is one; see the
    // thread-backed path for why a second record must not be created.
    let registry_task_id = match submitted_task_id {
        Some(task_id) => task_id,
        None => {
            let registry_task = task_registry.create_subagent_with_parent(
                description.clone(),
                task_agent_type,
                root_task_id.clone(),
            );
            arm_subagent_deadline(&task_registry, &registry_task.id, deadline_ms);
            registry_task.id
        }
    };
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
    let frozen_agent = match source.as_ref() {
        Some(source) => crate::subagent::restore_frozen_agent(&config, source),
        None => Ok(frozen_agent),
    };
    let frozen_agent = match frozen_agent.and_then(|frozen| {
        crate::subagent::apply_frozen_agent(&mut child_config, &subagent_type, frozen.as_ref())?;
        Ok(frozen)
    }) {
        Ok(frozen) => frozen,
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
    let delegation = frozen_agent
        .as_ref()
        .map(|frozen| frozen.delegation.clone())
        .unwrap_or(delegation);
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
    let (compatibility_model, compatibility_hash) = match compute_resumable_model_compatibility(
        source.as_ref(),
        &subagent_type,
        effective_model.as_deref(),
        isolation,
        &effective_cwd,
        worktree_binding.as_ref(),
        &delegation,
        &mcp_registry,
        &child_config.external_tools,
        frozen_agent.as_ref(),
    ) {
        Ok(compatibility) => compatibility,
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
        frozen_agent,
        model: compatibility_model,
        isolation,
        effective_cwd,
        worktree: worktree_binding,
        compatibility_hash,
    };
    let prompt_id = AgentPromptId::new();
    let parent_fence = activity_ingress
        .as_ref()
        .and_then(|ingress| ingress.parent_fence());
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
                .prepare_resume_with_parent_fence(input.clone(), parent_fence.as_ref())
                .or_else(|_| {
                    coordinator.prepare_resume_with_parent_fence(input, parent_fence.as_ref())
                })
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
            budget_reservation.clone(),
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
                budget_reservation.clone(),
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
    durable_reservation: Option<crate::child_budget_ledger::ChildBudgetReservation>,
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
                durable_reservation.clone(),
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
                    durable_reservation.clone(),
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
                durable_reservation.clone(),
            );
        }
    };
    let surface_subagent_id = match SurfaceSubagentId::try_new(registry_task_id.clone()) {
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
                durable_reservation.clone(),
            );
        }
    };
    // Synchronous children are part of the foreground surface generation.
    // Without a live surface ingress, fail closed instead of creating a
    // second presentation path through the task registry.
    let (activity_owner, activity_sink): (SubagentActivityOwner, Arc<dyn ChildAgentActivitySink>) =
        match activity_ingress {
            Some(activity_ingress) => (
                activity_ingress.owner(),
                Arc::new(RuntimeSubagentActivitySink {
                    ingress: activity_ingress,
                }),
            ),
            None => (
                SubagentActivityOwner::Generation {
                    operation_id: crate::runtime_surface::SurfaceOperationId::try_from_bytes(
                        *uuid::Uuid::now_v7().as_bytes(),
                    )
                    .expect("UUIDv7 produces a valid legacy operation id"),
                },
                Arc::new(LegacySubagentActivitySink),
            ),
        };
    // Allocate the child logical turn before the first activity envelope. The
    // exact value is then shared by the surface source cursor, runtime turn,
    // and child permission identity for this attempt.
    let child_turn_id = TurnId::new();
    let activity = Arc::new(ChildAgentActivityEmitter::new(
        SubagentActivityIdentity {
            task_id: surface_task_id.clone(),
            subagent_id: surface_subagent_id.clone(),
            attempt_id: prepared.attempt_id.clone(),
            turn_id: child_turn_id.clone(),
            owner: activity_owner,
        },
        activity_sink,
    ));
    if let Err(error) = activity.publish_payload(SubagentActivityPayload::Started {
        description: DisplayText::new(&description),
        batch_id: format!("sync-{registry_task_id}"),
        batch_size: 1,
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
            durable_reservation.clone(),
        );
    }
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
            child_task_id: Some(registry_task_id.as_str()),
            checkpoint_observer: Some(&checkpoint_observer),
            permission_handler: child_permission_handler,
            turn_id: Some(child_turn_id),
            executor: child_executor,
        });
        run_child_agent(&child_config, &child_request, &mut runtime)
    }));
    let worktree = worktree_execution.finish();
    let (output, panicked) = match child {
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
    let mut output = finalize_started_sync_subagent_with_revision(
        output,
        &coordinator,
        &lease,
        &shared_revision,
        &task_registry,
        &registry_task_id,
        panicked,
        durable_reservation.clone(),
    );
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
        output.result = ToolResult::indeterminate_after_start(
            &output.tool_request,
            output.event_error.clone().unwrap_or_else(|| {
                "child activity terminal could not be durably published".to_string()
            }),
        );
        output.status = RunStatus::Failed;
        output.task = output
            .task
            .take()
            .map(|task| task.with_status(RuntimeTaskStatus::Failed));
    }
    output
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

/// Records a caller-supplied execution deadline on a freshly created task.
///
/// The registry clamps it to the ancestors' deadlines, so a descendant can
/// never outlive the work that spawned it.
fn arm_subagent_deadline(task_registry: &TaskRegistry, task_id: &str, deadline_ms: Option<u64>) {
    let Some(deadline_ms) = deadline_ms else {
        return;
    };
    let deadline_at_ms = now_unix_ms().saturating_add(deadline_ms.min(i64::MAX as u64) as i64);
    let _ = task_registry.set_deadline_at(task_id, deadline_at_ms, "caller deadline_ms");
}

fn now_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

pub(crate) fn serialized_subagent_type(subagent_type: &SubagentType) -> Option<String> {
    // One source of truth: the role catalog owns every built-in identifier.
    match subagent_type {
        SubagentType::Custom(value) => Some(value.clone()),
        builtin => builtin
            .builtin()
            .map(|descriptor| descriptor.name.to_string()),
    }
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
    let requested_model = requested_model.map(orca_core::model::canonical_model_name);
    let source_model = source
        .compatibility
        .model
        .as_deref()
        .map(orca_core::model::canonical_model_name);
    if arguments.contains_key("model") && requested_model != source_model {
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
    durable_reservation: Option<crate::child_budget_ledger::ChildBudgetReservation>,
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
        durable_reservation,
    )
}

#[allow(clippy::too_many_arguments)]
fn finalize_started_sync_subagent_with_revision(
    mut output: RuntimeSubagentCallOutput,
    coordinator: &ChildAgentCoordinator,
    lease: &ContinuationLease,
    shared_revision: &Arc<Mutex<ContinuationRevision>>,
    task_registry: &TaskRegistry,
    registry_task_id: &str,
    panicked: bool,
    durable_reservation: Option<crate::child_budget_ledger::ChildBudgetReservation>,
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
            let mut message =
                continuation_error("failed to commit child continuation terminal", &error);
            if let Some(child_error) = output.event_error.as_deref() {
                message.push_str(&format!("\n\nChild error: {child_error}"));
            }
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
                durable_reservation,
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
        durable_reservation,
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
    durable_reservation: Option<crate::child_budget_ledger::ChildBudgetReservation>,
) {
    // Persist the receipt before making the task terminal/claimable. Otherwise
    // the parent could finish after seeing the result but before seeing its bill.
    //
    // Settle the durable reservation owned by the invocation (not the registry
    // clone) so the file-ledger write shares the terminal transition's
    // happens-before chain. Relying only on `task.budget_reservation` (read back
    // from the registry) leaves a window on slow filesystems where the parent
    // observes `Completed` before the receipt flush is visible.
    let receipt = output.child_budget_usage.or_else(|| {
        (output.result.terminal().started == orca_core::tool_types::ToolInvocationStarted::No)
            .then_some(orca_core::budget::BudgetUsage::default())
    });
    if let Some(reservation) = durable_reservation.or_else(|| {
        task_registry
            .get(registry_task_id)
            .and_then(|task| task.budget_reservation)
    }) && let Some(usage) = receipt
        && let Err(error) = reservation.settle(usage)
    {
        let message = format!("child budget settlement failed: {error}");
        output.result = ToolResult::indeterminate_after_start(&output.tool_request, &message);
        output.status = RunStatus::Failed;
        output.event_error = Some(message);
    }
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
            output = match validate_subagent_output_schema(&description, schema, &output) {
                Ok(normalized) => normalized,
                Err(mut error) => {
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
            };
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
) -> Result<String, String> {
    let Some(schema) = schema else {
        return Ok(output.to_string());
    };
    let value = serde_json::from_str(output).unwrap_or_else(|_| Value::String(output.to_string()));
    match validate_json_schema_subset(schema, &value, "$") {
        Ok(()) => Ok(output.to_string()),
        Err(outer_error) => {
            // Some providers occasionally serialize a structured response one
            // extra time, yielding a JSON string whose contents are the object
            // requested by the schema. Accept exactly that lossless transport
            // variation, but only when the decoded value itself validates.
            let Value::String(encoded) = value else {
                return Err(format!(
                    "subagent output schema validation failed for {description}: {outer_error}"
                ));
            };
            let Ok(decoded) = serde_json::from_str::<Value>(&encoded) else {
                return Err(format!(
                    "subagent output schema validation failed for {description}: {outer_error}"
                ));
            };
            validate_json_schema_subset(schema, &decoded, "$").map_err(|error| {
                format!("subagent output schema validation failed for {description}: {error}")
            })?;
            serde_json::to_string(&decoded).map_err(|error| {
                format!("subagent output schema normalization failed for {description}: {error}")
            })
        }
    }
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
    use crate::runtime_surface::TaskRevision;
    use orca_core::approval_types::ActionKind;
    use orca_core::tool_types::ToolName;

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
    fn output_schema_normalizes_one_extra_json_string_layer() {
        let schema = serde_json::json!({
            "type": "object",
            "required": ["file"],
            "properties": {"file": {"type": "string"}}
        });
        let value = serde_json::json!({"file": "src/lib.rs"});
        let encoded = serde_json::to_string(&value.to_string()).expect("encode transport string");

        let normalized = validate_subagent_output_schema("inspect", Some(&schema), &encoded)
            .expect("valid encoded object");

        assert_eq!(
            serde_json::from_str::<Value>(&normalized).expect("normalized json"),
            value
        );
    }

    #[test]
    fn output_schema_preserves_a_valid_json_string() {
        let schema = serde_json::json!({"type": "string"});
        let output = r#""already valid""#;

        assert_eq!(
            validate_subagent_output_schema("inspect", Some(&schema), output)
                .expect("valid string"),
            output
        );
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
            TurnId::new(),
            1,
            SubagentActivityOwner::DetachedTask {
                task_id: SurfaceTaskId::try_new("task-sync-activity").expect("task id"),
                task_revision: TaskRevision::try_new(1).expect("revision"),
                authority_digest: crate::runtime_surface::Sha256Digest::new([9; 32]),
            },
            SubagentActivityPayload::Started {
                description: DisplayText::new("inspect the runtime"),
                batch_id: "batch-runtime".to_string(),
                batch_size: 1,
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
    fn threaded_sync_surface_and_permission_use_the_registry_task_identity() {
        let ingress = Arc::new(RecordingActivityIngress::default());
        let task_registry = TaskRegistry::new("threaded-sync-canonical-identity".to_string());
        let task = task_registry.create_subagent("inspect identity".to_string(), None);
        let activity = threaded_sync_surface_activity(
            &task.id,
            &task.id,
            AgentAttemptId::new(),
            "inspect identity",
            "batch-identity",
            1,
            Some(ingress.clone()),
        )
        .expect("threaded activity");
        activity
            .surface_activity
            .emitter
            .publish_payload(SubagentActivityPayload::Started {
                description: DisplayText::new("inspect identity"),
                batch_id: "batch-identity".to_string(),
                batch_size: 1,
            })
            .expect("publish started activity");

        let events = ingress.events.lock().expect("recorded activity");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].task_id.as_str(), task.id);
        assert_eq!(events[0].subagent_id.as_str(), task.id);
        assert_eq!(
            activity
                .permission_identity
                .task_id
                .as_ref()
                .expect("permission task identity")
                .as_str(),
            task.id
        );
        assert_eq!(
            activity
                .permission_identity
                .agent_id
                .as_ref()
                .expect("permission agent identity")
                .as_str(),
            task.id
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
                    frozen_agent: None,
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

    #[test]
    fn resume_model_compatibility_treats_retired_flash_alias_as_canonical() {
        let registry = TaskRegistry::new("legacy-model-resume".to_string());
        let task = registry.create_subagent("resume legacy model".to_string(), None);
        let coordinator =
            ChildAgentCoordinator::with_owner_id(registry, "legacy-model-owner".to_string())
                .expect("coordinator");
        let source = coordinator
            .create(CreateContinuationInput {
                continuation_id: Some(AgentContinuationId::new()),
                parent_task_id: None,
                task_id: task.id,
                prompt_id: AgentPromptId::new(),
                compatibility: ContinuationCompatibility {
                    subagent_type: "general".to_string(),
                    frozen_agent: None,
                    model: Some(orca_core::model::LEGACY_FLASH_MODEL.to_string()),
                    isolation: SubagentIsolation::None,
                    effective_cwd: std::env::temp_dir().display().to_string(),
                    worktree: None,
                    compatibility_hash: crate::runtime_surface::Sha256Digest::new([4; 32]),
                },
            })
            .expect("prepared continuation");
        let tool_request = ToolRequest {
            id: "resume-legacy-model".to_string(),
            name: ToolName::Subagent,
            action: ActionKind::Agent,
            target: None,
            raw_arguments: Some(
                serde_json::json!({ "model": orca_core::model::FLASH_MODEL }).to_string(),
            ),
        };

        assert!(
            validate_resume_overrides(
                &tool_request,
                &SubagentType::General,
                Some(orca_core::model::FLASH_MODEL),
                SubagentIsolation::None,
                &source,
            )
            .is_ok()
        );
        assert!(
            validate_resume_overrides(
                &tool_request,
                &SubagentType::General,
                Some(orca_core::model::PRO_MODEL),
                SubagentIsolation::None,
                &source,
            )
            .unwrap_err()
            .contains("explicit model conflicts")
        );
    }

    #[test]
    fn queued_launch_policy_revalidation_rejects_revocation_and_expansion() {
        let original = crate::runtime_tool_call::tests::test_config();
        let snapshot = DelegationSnapshot::from_config(&original);
        let tool_request = ToolRequest {
            id: "queued-policy".into(),
            name: ToolName::Subagent,
            action: ActionKind::Agent,
            target: None,
            raw_arguments: Some(
                serde_json::json!({"description":"queued","prompt":"inspect"}).to_string(),
            ),
        };
        let mut request = crate::subagent::create_subagent_request(&tool_request);
        request.delegation = Some(snapshot.clone());
        assert!(queued_policy_is_unchanged(&request, &snapshot));

        let mut revoked = original.clone();
        revoked.subagents.delegation = orca_core::subagent_config::DelegationPolicy::Off;
        assert!(!queued_policy_is_unchanged(
            &request,
            &DelegationSnapshot::from_config(&revoked)
        ));

        let mut expanded = original;
        expanded.subagents.delegation = orca_core::subagent_config::DelegationPolicy::Explicit;
        assert!(!queued_policy_is_unchanged(
            &request,
            &DelegationSnapshot::from_config(&expanded)
        ));
    }
}
