use std::io::{self, Write};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::{Child, Command as ProcessCommand, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use base64::Engine;

#[cfg(windows)]
use serde::Serialize;
#[cfg(windows)]
use std::collections::BTreeMap;

use orca_platform::process::ProcessJob;
#[cfg(unix)]
use std::os::unix::process::CommandExt;

use orca_core::cancel::CancelToken;
use orca_core::capability::CapabilitySet;
use orca_core::config::RunConfig;
use orca_core::cost_types::UsageTotals;
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::event_sink::EventSink;
use orca_core::execution_broker::{ExecutionBroker, LaunchError};
use orca_core::subagent_types::SubagentType;
use orca_core::task_types::BackgroundTaskSummary;
use orca_core::thread_identity::TurnId;
use orca_core::tool_types;

use crate::agent_child::{
    ChildAgentExecutor, ChildAgentRequest, ChildAgentRuntime, ChildAgentRuntimeContext,
};
use crate::agent_continuation::{
    AgentContinuationId, AgentPromptId, AgentTerminal, ChildAgentCoordinator,
    ContinuationCompatibility, ContinuationLease, ContinuationProjection, ContinuationRevision,
    CreateContinuationInput, PreparedContinuation, ResumeContinuationInput, WorktreeBinding,
    compute_continuation_compatibility_hash,
};
use crate::agent_loop::execute_child_agent_loop;
use crate::child_agent_types::{
    ChildAgentActivityEmitter, ChildAgentActivitySink, SubagentActivityEvent,
    SubagentActivityIdentity, SubagentActivityOwner, SubagentActivityPayload, child_event_output,
};
use crate::child_agent_types::{ChildAgentCompatibilityIdentity, ChildAgentContinuationStart};
use crate::child_permission::{ChildPermissionIdentity, DetachedPermissionHandler};
use crate::hooks::HookRunner;
use crate::instructions;
use crate::lifecycle::{RuntimeSessionLifecycle, RuntimeTaskKind, RuntimeTaskStatus};
use crate::memory;
use crate::runtime_subagent_call::{
    ChildEventActivityObserver, append_worktree_outcome, build_checkpoint_observer,
    commit_continuation_write_with_retry, continuation_error, continuation_footer,
    serialized_subagent_type, validate_resume_overrides, validate_subagent_output_schema,
};
use crate::runtime_surface::{
    DisplayText, SurfaceOperationFence, SurfaceSubagentId, SurfaceSubagentTerminalStatus,
    SurfaceTaskId, TaskRevision,
};
use crate::subagent::{self, SubagentIsolation};
use crate::subagent_event_relay::RelayRecord;
use crate::tasks::TaskRegistry;
use crate::worktree::WorktreeGuard;

#[cfg(windows)]
const WINDOWS_RUNNER_PROTOCOL_VERSION: u32 = 1;

#[cfg(windows)]
#[derive(Debug, Serialize)]
struct WindowsRunnerLaunchRequest {
    version: u32,
    program: String,
    args: Vec<String>,
    cwd: String,
    env: BTreeMap<String, Option<String>>,
    job_name: Option<String>,
    forward_stdin: bool,
}

#[derive(Clone, Debug)]
pub struct AsyncSubagentWorktree {
    pub repo_root: PathBuf,
    pub path: PathBuf,
}

pub struct AsyncSubagentWorkerInput {
    pub config: RunConfig,
    pub cwd: PathBuf,
    pub child_cwd: PathBuf,
    pub task_session_id: String,
    pub agent_id: String,
    pub request: subagent::SubagentRequest,
    pub child_depth: u32,
    pub worktree: Option<AsyncSubagentWorktree>,
    pub permission_response_public_key: [u8; 32],
    /// Logical child turn allocated by the admitting actor.  Detached relay
    /// records must keep this identity across the parent/worker boundary.
    pub child_turn_id: TurnId,
    /// The actor already committed `Started` at source sequence one before
    /// spawning this worker.  Direct test/legacy launches may leave this false
    /// and let the worker emit the start itself.
    pub activity_start_precommitted: bool,
}

pub(crate) struct AsyncSubagentWorkerContext {
    pub input: AsyncSubagentWorkerInput,
    pub child_executor: ChildAgentExecutor<io::Sink>,
}

pub(crate) struct AsyncSubagentLaunchContext<'a> {
    pub config: &'a RunConfig,
    pub cwd: &'a Path,
    pub tool_request: &'a tool_types::ToolRequest,
    pub request: subagent::SubagentRequest,
    pub subagent_depth: u32,
    pub task_registry: &'a TaskRegistry,
    pub root_task_id: Option<&'a str>,
    /// Parent operation identity retained by detached activity after the
    /// admitting generation leaves the resident actor.
    pub parent_fence: Option<SurfaceOperationFence>,
    /// Parent actor ingress used to commit the detached `Started` fact before
    /// any worker process is spawned. The relay worker starts at source
    /// sequence two after this durable pre-launch commit.
    pub activity_ingress: Option<Arc<dyn crate::runtime_surface::RuntimeSubagentActivityIngress>>,
}

pub(crate) struct AsyncSubagentLaunchOutput {
    pub(crate) result: tool_types::ToolResult,
    pub(crate) task: Option<BackgroundTaskSummary>,
}

struct AsyncSubagentWorkerSpawnContext<'a> {
    config: &'a RunConfig,
    cwd: &'a Path,
    child_cwd: &'a Path,
    task_session_id: &'a str,
    agent_id: &'a str,
    request: &'a subagent::SubagentRequest,
    child_depth: u32,
    worktree: Option<&'a AsyncSubagentWorktree>,
    permission_response_public_key: &'a [u8; 32],
    child_turn_id: &'a TurnId,
    activity_start_precommitted: bool,
}

/// Bridges the actor-owned surface ingress into the emitter abstraction used
/// by child execution.  This is only used for the pre-launch `Started` fact
/// and for failures before a worker process can adopt the task.
#[derive(Clone)]
struct SurfaceActivitySink {
    ingress: Arc<dyn crate::runtime_surface::RuntimeSubagentActivityIngress>,
}

impl ChildAgentActivitySink for SurfaceActivitySink {
    fn publish(&self, event: SubagentActivityEvent) -> io::Result<()> {
        self.ingress.commit_activity(event)
    }
}

struct AsyncLaunchWorktree {
    child_cwd: PathBuf,
    worker: Option<AsyncSubagentWorktree>,
    fresh_guard: Option<WorktreeGuard>,
}

/// Lease-fenced detached activity publisher. The relay is the source delivery
/// boundary; the task registry remains only a repairable post-append mirror.
#[derive(Clone)]
struct DetachedRelayActivitySink {
    task_registry: TaskRegistry,
    task_lease: crate::tasks::TaskLease,
    task_id: String,
    attempt_id: String,
}

impl ChildAgentActivitySink for DetachedRelayActivitySink {
    fn publish(&self, event: SubagentActivityEvent) -> io::Result<()> {
        if !event.verify_digest() || event.attempt_id.as_str() != self.attempt_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "detached activity envelope failed digest or attempt validation",
            ));
        }
        let payload = serde_json::to_vec(&event).map_err(io::Error::other)?;
        let record = RelayRecord::new(
            &crate::subagent_event_relay::RelayLease::new(
                self.task_id.clone(),
                crate::subagent_event_relay::RelayTaskType::Subagent,
                self.task_lease.owner_id(),
                self.task_lease.epoch(),
                self.attempt_id.clone(),
            )
            .map_err(io::Error::other)?,
            event.source_sequence,
            event.surface_commit_id,
            payload,
        );
        self.task_registry
            .append_subagent_event_with_lease(
                &self.task_lease,
                &self.task_id,
                &self.attempt_id,
                record,
            )
            .map(|_| ())
            .map_err(io::Error::other)
    }
}

impl AsyncLaunchWorktree {
    fn finish_fresh(self) -> Option<crate::worktree::WorktreeOutcome> {
        self.fresh_guard.and_then(|guard| guard.finish().ok())
    }

    fn detach(self) {
        if let Some(guard) = self.fresh_guard {
            std::mem::forget(guard);
        }
    }
}

struct AsyncLeaseHeartbeat {
    stop: mpsc::Sender<()>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl AsyncLeaseHeartbeat {
    fn start(
        task_registry: TaskRegistry,
        task_lease: crate::tasks::TaskLease,
        agent_id: String,
        coordinator: ChildAgentCoordinator,
        continuation_lease: ContinuationLease,
        continuation_revision: Arc<Mutex<ContinuationRevision>>,
    ) -> Self {
        let (stop, receiver) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            loop {
                match receiver.recv_timeout(Duration::from_secs(5)) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
                if task_registry
                    .renew_task_lease(&task_lease, &agent_id)
                    .is_err()
                {
                    break;
                }
                let Ok(mut revision) = continuation_revision.lock() else {
                    break;
                };
                let projection = match coordinator.renew(&continuation_lease, *revision) {
                    Ok(projection) => projection,
                    Err(_) => break,
                };
                *revision = projection.revision;
            }
        });
        Self {
            stop,
            worker: Some(worker),
        }
    }

    fn stop(mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub fn run_async_subagent_worker(input: AsyncSubagentWorkerInput) -> i32 {
    run_async_subagent_worker_with_executor(AsyncSubagentWorkerContext {
        input,
        child_executor: execute_child_agent_loop,
    })
}

pub(crate) fn run_async_subagent_worker_with_executor(context: AsyncSubagentWorkerContext) -> i32 {
    let AsyncSubagentWorkerContext {
        input,
        child_executor,
    } = context;
    let AsyncSubagentWorkerInput {
        config,
        cwd,
        child_cwd,
        task_session_id,
        agent_id,
        request,
        child_depth,
        worktree,
        permission_response_public_key,
        child_turn_id,
        activity_start_precommitted,
    } = input;
    let owns_worktree = request.resume_from.is_none();
    let task_registry = match wait_for_async_subagent_adoption(&task_session_id, &cwd, &agent_id) {
        Ok(registry) => registry,
        Err(_) => return 1,
    };
    let task_lease = match task_registry.acquire_task_lease(&agent_id) {
        Ok(lease) => lease,
        Err(_) => return 1,
    };
    if task_registry
        .mark_running_with_lease(&task_lease, &agent_id)
        .is_err()
    {
        return 1;
    }
    let coordinator = match ChildAgentCoordinator::with_owner_id(
        task_registry.clone(),
        format!("async-subagent:{}:{agent_id}", std::process::id()),
    ) {
        Ok(coordinator) => coordinator,
        Err(error) => {
            let mut message = continuation_error("failed to initialize async continuation", &error);
            let worktree = finish_async_worker_worktree(worktree, owns_worktree);
            append_worktree_outcome(&mut message, worktree.as_ref());
            let _ = task_registry.fail_with_usage_and_lease(&task_lease, &agent_id, message, None);
            return 1;
        }
    };
    let prepared = match coordinator.prepared(&agent_id) {
        Ok(prepared) => prepared,
        Err(error) => {
            let mut message = continuation_error("failed to load async continuation", &error);
            let worktree = finish_async_worker_worktree(worktree, owns_worktree);
            append_worktree_outcome(&mut message, worktree.as_ref());
            let _ = task_registry.fail_with_usage_and_lease(&task_lease, &agent_id, message, None);
            return 1;
        }
    };
    let detached_binding = match task_registry.detached_subagent_binding(&agent_id) {
        Ok(Some(binding)) => binding,
        Ok(None) => {
            let message = "detached subagent owner binding is missing or stale".to_string();
            let _ = task_registry.fail_with_usage_and_lease(&task_lease, &agent_id, message, None);
            return 1;
        }
        Err(error) => {
            let message = format!("failed to load detached subagent owner binding: {error}");
            let _ = task_registry.fail_with_usage_and_lease(&task_lease, &agent_id, message, None);
            return 1;
        }
    };
    if detached_binding.permission_response_public_key != permission_response_public_key {
        let message =
            "detached subagent response key does not match the actor-issued launch material"
                .to_string();
        let _ = task_registry.fail_with_usage_and_lease(
            &task_lease,
            &agent_id,
            message.to_string(),
            None,
        );
        return 1;
    }
    let continuation_lease = match coordinator
        .acquire(&prepared)
        .or_else(|_| coordinator.acquire(&prepared))
    {
        Ok(lease) => lease,
        Err(error) => {
            let mut message = continuation_error("failed to acquire async continuation", &error);
            let worktree = finish_async_worker_worktree(worktree, owns_worktree);
            append_worktree_outcome(&mut message, worktree.as_ref());
            let _ = task_registry.fail_with_usage_and_lease(&task_lease, &agent_id, message, None);
            return 1;
        }
    };
    let continuation_start = match prepared.checkpoint.clone() {
        Some(checkpoint) => match ChildAgentContinuationStart::new(
            prepared.continuation_id.clone(),
            prepared.attempt_id.clone(),
            prepared.prompt_id.clone(),
            checkpoint,
            ChildAgentCompatibilityIdentity::new(prepared.compatibility.compatibility_hash),
        ) {
            Ok(start) => Some(start),
            Err(error) => {
                let mut message =
                    continuation_error("failed to construct async continuation start", &error);
                let worktree = finish_async_worker_worktree(worktree, owns_worktree);
                append_worktree_outcome(&mut message, worktree.as_ref());
                let projection = commit_async_terminal(
                    &coordinator,
                    &continuation_lease,
                    None,
                    AgentTerminal::Failed {
                        error: message.clone(),
                    },
                )
                .ok();
                let message = append_projection_footer(message, projection.as_ref());
                let _ =
                    task_registry.fail_with_usage_and_lease(&task_lease, &agent_id, message, None);
                return 1;
            }
        },
        None => None,
    };
    let (checkpoint_observer, shared_revision) =
        build_checkpoint_observer(coordinator.clone(), continuation_lease.clone(), &prepared);
    let heartbeat = AsyncLeaseHeartbeat::start(
        task_registry.clone(),
        task_lease.clone(),
        agent_id.clone(),
        coordinator.clone(),
        continuation_lease.clone(),
        Arc::clone(&shared_revision),
    );
    let instructions = instructions::load_for_cwd_or_default(&cwd);
    let memory = memory::load_for_cwd(&cwd);
    let hooks = HookRunner::new_with_capabilities(
        config.hooks.clone(),
        CapabilitySet::for_approval_mode(config.approval_mode),
    );
    let mcp_registry = orca_mcp::initialize_registry(&config.mcp_servers);
    let cancel = CancelToken::new();
    let child_request = ChildAgentRequest {
        prompt: request.prompt,
        subagent_type: request.subagent_type,
        model: request.model,
        depth: child_depth,
        emit_deltas: true,
        allowed_tools: None,
        tool_policy_label: None,
        workflow_ipc: None,
        continuation: continuation_start,
    };
    let surface_task_id = SurfaceTaskId::try_new(agent_id.clone())
        .expect("async task registry created a non-empty task id");
    let activity_sink: Arc<dyn ChildAgentActivitySink> = Arc::new(DetachedRelayActivitySink {
        task_registry: task_registry.clone(),
        task_lease: task_lease.clone(),
        task_id: agent_id.clone(),
        attempt_id: prepared.attempt_id.as_str().to_string(),
    });
    // The admitting actor allocates this identity before process spawn.  Keep
    // it unchanged for permission requests and every subsequent relay event.
    let activity = Arc::new(
        ChildAgentActivityEmitter::new_with_revision_source_and_sequence(
            SubagentActivityIdentity {
                task_id: surface_task_id.clone(),
                subagent_id: SurfaceSubagentId::try_new(agent_id.clone())
                    .expect("async task registry created a non-empty task id"),
                attempt_id: prepared.attempt_id.clone(),
                turn_id: child_turn_id.clone(),
                owner: SubagentActivityOwner::DetachedTask {
                    task_id: surface_task_id.clone(),
                    task_revision: detached_binding.task_revision,
                    authority_digest: detached_binding.authority_digest,
                },
            },
            activity_sink,
            Arc::new(std::sync::atomic::AtomicU64::new(1)),
            if activity_start_precommitted { 2 } else { 1 },
        ),
    );
    if !activity_start_precommitted
        && let Err(error) = activity.publish_payload(SubagentActivityPayload::Started {
            description: DisplayText::new(&request.description),
            batch_id: format!("async-{agent_id}"),
            batch_size: 1,
        })
    {
        let worktree = finish_async_worker_worktree(worktree, owns_worktree);
        let mut message = format!("failed to publish async subagent start: {error}");
        append_worktree_outcome(&mut message, worktree.as_ref());
        let _ = task_registry.fail_with_usage_and_lease(&task_lease, &agent_id, message, None);
        return 1;
    }
    let child_permission_handler = Arc::new(DetachedPermissionHandler::new(
        task_registry.clone(),
        detached_binding.clone(),
        ChildPermissionIdentity::new_with_task_revision(
            surface_task_id.clone(),
            detached_binding.task_revision,
            SurfaceSubagentId::try_new(agent_id.clone())
                .expect("async task registry created a non-empty task id"),
            child_turn_id.clone(),
            activity.revision_source(),
        ),
        cancel.clone(),
    ));
    let mut child_events = EventFactory::new(format!("subagent-{agent_id}"));
    let mut child_lifecycle = RuntimeSessionLifecycle::new(format!("subagent-{agent_id}"));
    child_lifecycle.start_task(RuntimeTaskKind::Subagent);
    let mut child_sink = EventSink::new(child_event_output(), config.output_format).with_observer(
        Arc::new(ChildEventActivityObserver {
            emitter: Arc::clone(&activity),
        }),
    );
    let child = panic::catch_unwind(AssertUnwindSafe(|| {
        let mut child_runtime = ChildAgentRuntime::new(ChildAgentRuntimeContext {
            cwd: &child_cwd,
            events: &mut child_events,
            sink: &mut child_sink,
            instructions: &instructions,
            memory: &memory,
            mcp_registry: &mcp_registry,
            hooks: &hooks,
            cancel: &cancel,
            lifecycle: Some(&mut child_lifecycle),
            task_registry: Some(&task_registry),
            root_task_id: Some(&agent_id),
            checkpoint_observer: Some(&checkpoint_observer),
            permission_handler: Some(
                child_permission_handler
                    as Arc<
                        dyn crate::runtime_permission::RuntimePermissionRequestHandler
                            + Send
                            + Sync,
                    >,
            ),
            turn_id: Some(child_turn_id),
            executor: child_executor,
        });
        crate::agent_child::run_child_agent(&config, &child_request, &mut child_runtime)
    }));
    heartbeat.stop();
    let (child, child_cost_tracker) = match child {
        Ok((child, cost_tracker)) => (child, cost_tracker),
        Err(payload) => {
            let mut message = format!(
                "Async subagent worker panicked after execution started: {}. Inspect external state before retrying.",
                panic_payload_message(payload)
            );
            let worktree = finish_async_worker_worktree(worktree, owns_worktree);
            append_worktree_outcome(&mut message, worktree.as_ref());
            let projection = match commit_async_terminal(
                &coordinator,
                &continuation_lease,
                Some(&shared_revision),
                AgentTerminal::Indeterminate {
                    reason: message.clone(),
                },
            ) {
                Ok(projection) => Some(projection),
                Err(error) => {
                    message.push_str(&format!(
                        "\n\nfailed to commit async continuation terminal: {error}"
                    ));
                    None
                }
            };
            if projection.is_some()
                && let Err(error) = activity.publish_payload(SubagentActivityPayload::Completed {
                    status: SurfaceSubagentTerminalStatus::Failed,
                    output: None,
                    error: Some(DisplayText::new(&message)),
                    usage: None,
                })
            {
                message.push_str(&format!(
                    "\n\nchild activity terminal could not be published: {error}"
                ));
            }
            let message = append_projection_footer(message, projection.as_ref());
            let message = async_subagent_result_payload(message, None);
            let _ = task_registry.fail_with_usage_and_lease(&task_lease, &agent_id, message, None);
            return 1;
        }
    };
    let terminal_status = match child.status {
        RunStatus::Success => SurfaceSubagentTerminalStatus::Completed,
        RunStatus::Cancelled => SurfaceSubagentTerminalStatus::Cancelled,
        RunStatus::Failed | RunStatus::ApprovalRequired | RunStatus::VerificationFailed => {
            SurfaceSubagentTerminalStatus::Failed
        }
    };
    let completed_task = child_lifecycle
        .finish_task(child.status)
        .cloned()
        .unwrap_or_else(|| {
            child_lifecycle.active_task().cloned().unwrap_or_else(|| {
                RuntimeSessionLifecycle::new(format!("subagent-{agent_id}"))
                    .start_task(RuntimeTaskKind::Subagent)
                    .clone()
            })
        });
    let worktree = finish_async_worker_worktree(worktree, owns_worktree);
    let usage = usage_totals_if_non_empty(child_cost_tracker.totals());
    if child.status == RunStatus::Success {
        let mut output = child
            .final_message
            .unwrap_or_else(|| "(subagent completed without a final message)".to_string());
        if let Err(mut error) =
            validate_subagent_output_schema(&request.description, request.schema.as_ref(), &output)
        {
            append_worktree_outcome(&mut error, worktree.as_ref());
            let projection = match commit_async_terminal(
                &coordinator,
                &continuation_lease,
                Some(&shared_revision),
                AgentTerminal::Failed {
                    error: error.clone(),
                },
            ) {
                Ok(projection) => projection,
                Err(commit_error) => {
                    let error = format!(
                        "{error}\n\nfailed to commit async continuation terminal: {commit_error}"
                    );
                    let failed_task = completed_task.with_status(RuntimeTaskStatus::Failed);
                    let error = async_subagent_result_payload(error, Some(failed_task.payload()));
                    let _ = task_registry.fail_with_usage_and_lease(
                        &task_lease,
                        &agent_id,
                        error,
                        usage,
                    );
                    return 1;
                }
            };
            let mut error = append_projection_footer(error, Some(&projection));
            if let Err(activity_error) =
                activity.publish_payload(SubagentActivityPayload::Completed {
                    status: SurfaceSubagentTerminalStatus::Failed,
                    output: None,
                    error: Some(DisplayText::new(&error)),
                    usage: usage.map(|_| child_cost_tracker.totals()),
                })
            {
                error.push_str(&format!(
                    "\n\nchild activity terminal could not be durably published: {activity_error}"
                ));
            }
            let failed_task = completed_task.with_status(RuntimeTaskStatus::Failed);
            let error = async_subagent_result_payload(error, Some(failed_task.payload()));
            let _ = task_registry.fail_with_usage_and_lease(&task_lease, &agent_id, error, usage);
            return 1;
        }
        append_worktree_outcome(&mut output, worktree.as_ref());
        let projection = match commit_async_terminal(
            &coordinator,
            &continuation_lease,
            Some(&shared_revision),
            AgentTerminal::Completed {
                result: Some(output.clone()),
            },
        ) {
            Ok(projection) => projection,
            Err(error) => {
                let error =
                    continuation_error("failed to commit async continuation terminal", &error);
                let error = async_subagent_result_payload(error, Some(completed_task.payload()));
                let _ =
                    task_registry.fail_with_usage_and_lease(&task_lease, &agent_id, error, usage);
                return 1;
            }
        };
        output = append_projection_footer(output, Some(&projection));
        if let Err(error) = activity.publish_payload(SubagentActivityPayload::Completed {
            status: terminal_status.clone(),
            output: Some(DisplayText::new(&output)),
            error: None,
            usage: Some(child_cost_tracker.totals()),
        }) {
            let error = format!("child activity terminal could not be durably published: {error}");
            let error = async_subagent_result_payload(
                error,
                Some(
                    completed_task
                        .with_status(RuntimeTaskStatus::Failed)
                        .payload(),
                ),
            );
            let _ = task_registry.fail_with_usage_and_lease(&task_lease, &agent_id, error, usage);
            return 1;
        }
        let output = async_subagent_result_payload(output, Some(completed_task.payload()));
        if task_registry
            .complete_with_usage_and_lease(&task_lease, &agent_id, output, usage)
            .is_ok()
        {
            return 0;
        }
    } else {
        let mut error = child
            .error
            .or(child.final_message)
            .unwrap_or_else(|| format!("subagent ended with status {:?}", child.status));
        append_worktree_outcome(&mut error, worktree.as_ref());
        let terminal = match child.status {
            RunStatus::Cancelled => AgentTerminal::Cancelled {
                reason: Some(error.clone()),
            },
            _ => AgentTerminal::Failed {
                error: error.clone(),
            },
        };
        let projection = match commit_async_terminal(
            &coordinator,
            &continuation_lease,
            Some(&shared_revision),
            terminal,
        ) {
            Ok(projection) => projection,
            Err(commit_error) => {
                let error = format!(
                    "{error}\n\nfailed to commit async continuation terminal: {commit_error}"
                );
                let error = async_subagent_result_payload(
                    error,
                    Some(
                        completed_task
                            .with_status(RuntimeTaskStatus::Failed)
                            .payload(),
                    ),
                );
                let _ =
                    task_registry.fail_with_usage_and_lease(&task_lease, &agent_id, error, usage);
                return 1;
            }
        };
        let mut error = append_projection_footer(error, Some(&projection));
        if let Err(activity_error) = activity.publish_payload(SubagentActivityPayload::Completed {
            status: terminal_status,
            output: None,
            error: Some(DisplayText::new(&error)),
            usage: Some(child_cost_tracker.totals()),
        }) {
            error.push_str(&format!(
                "\n\nchild activity terminal could not be durably published: {activity_error}"
            ));
        }
        let error = async_subagent_result_payload(error, Some(completed_task.payload()));
        if task_registry
            .fail_with_usage_and_lease(&task_lease, &agent_id, error, usage)
            .is_ok()
        {
            return 1;
        }
    }
    1
}

pub(crate) fn launch_async_subagent(
    context: AsyncSubagentLaunchContext<'_>,
) -> AsyncSubagentLaunchOutput {
    let AsyncSubagentLaunchContext {
        config,
        cwd,
        tool_request,
        request,
        subagent_depth,
        task_registry,
        root_task_id,
        parent_fence,
        activity_ingress,
    } = context;
    let mut request = subagent::with_delegation_snapshot(
        request,
        orca_core::config::DelegationSnapshot::from_config(config),
    );
    if task_registry.is_process_local() {
        return AsyncSubagentLaunchOutput {
            result: tool_types::ToolResult::failed(
                tool_request,
                "async subagents require persistent task ownership; use sync mode for a history-disabled run",
                None,
            ),
            task: None,
        };
    }
    if parent_fence.is_none() {
        return AsyncSubagentLaunchOutput {
            result: tool_types::ToolResult::failed(
                tool_request,
                "async subagents require an actor-owned parent operation fence",
                None,
            ),
            task: None,
        };
    }
    let Some(activity_ingress) = activity_ingress else {
        return AsyncSubagentLaunchOutput {
            result: tool_types::ToolResult::failed(
                tool_request,
                "async subagents require an actor-owned activity ingress for pre-launch Started",
                None,
            ),
            task: None,
        };
    };
    let coordinator = match ChildAgentCoordinator::new(task_registry.clone()) {
        Ok(coordinator) => coordinator,
        Err(error) => {
            return AsyncSubagentLaunchOutput {
                result: tool_types::ToolResult::failed(
                    tool_request,
                    continuation_error("failed to initialize async continuation", &error),
                    None,
                ),
                task: None,
            };
        }
    };
    let source = match request.resume_from.as_deref() {
        Some(selector) => match coordinator.prepared(selector) {
            Ok(source) => Some(source),
            Err(error) => {
                return AsyncSubagentLaunchOutput {
                    result: tool_types::ToolResult::failed(
                        tool_request,
                        continuation_error("failed to resolve async continuation", &error),
                        None,
                    ),
                    task: None,
                };
            }
        },
        None => None,
    };
    let agent_type = source
        .as_ref()
        .map(|source| source.compatibility.subagent_type.clone())
        .or_else(|| serialized_subagent_type(&request.subagent_type));
    let task = task_registry.create_subagent_with_parent(
        request.description.clone(),
        agent_type,
        root_task_id.map(str::to_string),
    );
    let agent_id = task.id.clone();
    if task_registry.is_cancelled(&agent_id) {
        let _ = task_registry.stop(
            &agent_id,
            "Task stopped because its foreground owner was cancelled".to_string(),
        );
        return async_launch_output(
            task_registry,
            &agent_id,
            tool_types::ToolResult::cancelled_before_start(
                tool_request,
                "the foreground operation was cancelled before the async subagent started",
            ),
        );
    }
    if let Some(source) = source.as_ref()
        && let Err(error) = validate_resume_overrides(
            tool_request,
            &request.subagent_type,
            request.model.as_deref(),
            request.isolation,
            source,
        )
    {
        let _ = task_registry.fail(&agent_id, error.clone());
        return async_launch_output(
            task_registry,
            &agent_id,
            tool_types::ToolResult::failed(tool_request, error, None),
        );
    }
    request.subagent_type = source
        .as_ref()
        .map(|source| SubagentType::from_str(&source.compatibility.subagent_type))
        .unwrap_or(request.subagent_type);
    request.model = source
        .as_ref()
        .map(|source| source.compatibility.model.clone())
        .unwrap_or(request.model);
    request.isolation = source
        .as_ref()
        .map(|source| source.compatibility.isolation)
        .unwrap_or(request.isolation);
    let delegation = request
        .delegation
        .clone()
        .unwrap_or_else(|| orca_core::config::DelegationSnapshot::from_config(config));
    let mut child_config = config.clone();
    delegation.apply_to(&mut child_config, request.model.clone());
    request.model = child_config.model.as_option();
    let launch_worktree =
        match prepare_async_launch_worktree(source.as_ref(), request.isolation, cwd) {
            Ok(worktree) => worktree,
            Err(error) => {
                let _ = task_registry.fail(&agent_id, error.clone());
                return async_launch_output(
                    task_registry,
                    &agent_id,
                    tool_types::ToolResult::failed(tool_request, error, None),
                );
            }
        };
    let mcp_registry = orca_mcp::initialize_registry(&child_config.mcp_servers);
    let worktree_binding = launch_worktree
        .worker
        .as_ref()
        .map(|worktree| WorktreeBinding {
            repo_root: worktree.repo_root.display().to_string(),
            path: worktree.path.display().to_string(),
        });
    let compatibility_hash = match compute_continuation_compatibility_hash(
        &request.subagent_type,
        request.model.as_deref(),
        request.isolation,
        &launch_worktree.child_cwd.display().to_string(),
        worktree_binding.as_ref(),
        &delegation,
        &mcp_registry,
        &child_config.external_tools,
    ) {
        Ok(hash) => hash,
        Err(error) => {
            let worktree = launch_worktree.finish_fresh();
            let mut error = continuation_error("failed to compute async compatibility", &error);
            append_worktree_outcome(&mut error, worktree.as_ref());
            let _ = task_registry.fail(&agent_id, error.clone());
            return async_launch_output(
                task_registry,
                &agent_id,
                tool_types::ToolResult::failed(tool_request, error, None),
            );
        }
    };
    let compatibility = ContinuationCompatibility {
        subagent_type: serialized_subagent_type(&request.subagent_type)
            .unwrap_or_else(|| "general".to_string()),
        model: request.model.clone(),
        isolation: request.isolation,
        effective_cwd: launch_worktree.child_cwd.display().to_string(),
        worktree: worktree_binding,
        compatibility_hash,
    };
    let prompt_id = AgentPromptId::new();
    let prepared = match source.as_ref() {
        Some(_) => coordinator.prepare_resume(ResumeContinuationInput {
            selector: request
                .resume_from
                .clone()
                .expect("resolved async resume source has selector"),
            parent_task_id: root_task_id.map(str::to_string),
            task_id: agent_id.clone(),
            prompt_id,
            compatibility,
        }),
        None => coordinator.create(CreateContinuationInput {
            continuation_id: Some(AgentContinuationId::new()),
            parent_task_id: root_task_id.map(str::to_string),
            task_id: agent_id.clone(),
            prompt_id,
            compatibility,
        }),
    };
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            let worktree = launch_worktree.finish_fresh();
            let mut error = continuation_error("failed to prepare async continuation", &error);
            append_worktree_outcome(&mut error, worktree.as_ref());
            let _ = task_registry.fail(&agent_id, error.clone());
            return async_launch_output(
                task_registry,
                &agent_id,
                tool_types::ToolResult::failed(tool_request, error, None),
            );
        }
    };
    // Persist the actor-issued detached owner before the worker is spawned.
    // The worker later reloads this binding after adoption; configuration
    // compatibility hashes are not used as authority credentials.
    let detached_binding = match task_registry.register_detached_subagent_binding(
        &agent_id,
        &agent_id,
        prepared.attempt_id.clone(),
        TaskRevision::try_new(1).expect("one is a valid task revision"),
        parent_fence,
    ) {
        Ok(binding) => binding,
        Err(error) => {
            let worktree = launch_worktree.finish_fresh();
            let mut error = format!("failed to persist detached subagent owner: {error}");
            append_worktree_outcome(&mut error, worktree.as_ref());
            let projection = coordinator
                .commit_prepared_terminal(
                    &prepared,
                    AgentTerminal::Failed {
                        error: error.clone(),
                    },
                )
                .ok();
            let error = append_projection_footer(error, projection.as_ref());
            let _ = task_registry.fail(&agent_id, error.clone());
            return async_launch_output(
                task_registry,
                &agent_id,
                tool_types::ToolResult::failed(tool_request, error, None),
            );
        }
    };
    let child_turn_id = TurnId::new();
    let parent_activity = Arc::new(ChildAgentActivityEmitter::new(
        SubagentActivityIdentity {
            task_id: SurfaceTaskId::try_new(agent_id.clone())
                .expect("async task registry created a non-empty task id"),
            subagent_id: SurfaceSubagentId::try_new(agent_id.clone())
                .expect("async task registry created a non-empty task id"),
            attempt_id: prepared.attempt_id.clone(),
            turn_id: child_turn_id.clone(),
            owner: SubagentActivityOwner::DetachedTask {
                task_id: SurfaceTaskId::try_new(agent_id.clone())
                    .expect("async task registry created a non-empty task id"),
                task_revision: detached_binding.task_revision,
                authority_digest: detached_binding.authority_digest,
            },
        },
        Arc::new(SurfaceActivitySink {
            ingress: activity_ingress.clone(),
        }),
    ));
    if let Err(error) = parent_activity.publish_payload(SubagentActivityPayload::Started {
        description: DisplayText::new(&request.description),
        batch_id: format!("async-{agent_id}"),
        batch_size: 1,
    }) {
        let worktree = launch_worktree.finish_fresh();
        let mut message = format!("failed to commit async subagent Started activity: {error}");
        append_worktree_outcome(&mut message, worktree.as_ref());
        // The surface sequence starts at one. If Started itself was rejected,
        // publishing a sequence-two terminal would create an orphaned gap.
        // Keep the durable continuation/task state fail-closed and let the
        // rejected Started remain absent from the surface projection.
        let projection = match coordinator.commit_prepared_terminal(
            &prepared,
            AgentTerminal::Failed {
                error: message.clone(),
            },
        ) {
            Ok(projection) => Some(projection),
            Err(commit_error) => {
                message.push_str(&format!(
                    "\n\nfailed to commit async continuation terminal: {commit_error}"
                ));
                None
            }
        };
        let message = append_projection_footer(message, projection.as_ref());
        let _ = task_registry.fail(&agent_id, message.clone());
        return async_launch_output(
            task_registry,
            &agent_id,
            tool_types::ToolResult::failed(tool_request, message, None),
        );
    }
    if let Err(error) = task_registry.mark_worker_spawned(&agent_id, 0) {
        let worktree = launch_worktree.finish_fresh();
        let mut error = error;
        append_worktree_outcome(&mut error, worktree.as_ref());
        error = finish_async_launch_failure(&coordinator, &prepared, &parent_activity, error);
        let _ = task_registry.fail(&agent_id, error.clone());
        return async_launch_output(
            task_registry,
            &agent_id,
            tool_types::ToolResult::failed(tool_request, error, None),
        );
    }
    match spawn_async_subagent_worker(AsyncSubagentWorkerSpawnContext {
        config: &child_config,
        cwd,
        child_cwd: &launch_worktree.child_cwd,
        task_session_id: task_registry.session_id(),
        agent_id: &agent_id,
        request: &request,
        child_depth: subagent_depth + 1,
        worktree: launch_worktree.worker.as_ref(),
        permission_response_public_key: &detached_binding.permission_response_public_key,
        child_turn_id: &child_turn_id,
        activity_start_precommitted: true,
    }) {
        Ok((child, process_job)) => {
            if let Err(error) =
                task_registry.adopt_subagent_worker_with_job(&agent_id, child, process_job)
            {
                let worktree = launch_worktree.finish_fresh();
                let mut error = format!("failed to own async subagent worker: {error}");
                append_worktree_outcome(&mut error, worktree.as_ref());
                error =
                    finish_async_launch_failure(&coordinator, &prepared, &parent_activity, error);
                let _ = task_registry.fail(&agent_id, error.clone());
                return async_launch_output(
                    task_registry,
                    &agent_id,
                    tool_types::ToolResult::failed(tool_request, error, None),
                );
            }
            launch_worktree.detach();
        }
        Err(error) => {
            let worktree = launch_worktree.finish_fresh();
            let mut error = format!("failed to start async subagent worker: {error}");
            append_worktree_outcome(&mut error, worktree.as_ref());
            error = finish_async_launch_failure(&coordinator, &prepared, &parent_activity, error);
            let _ = task_registry.fail(&agent_id, error.clone());
            return async_launch_output(
                task_registry,
                &agent_id,
                tool_types::ToolResult::failed(tool_request, error, None),
            );
        }
    }

    let projection = coordinator.projection(&agent_id).ok();
    let output = serde_json::json!({
        "status": "async_launched",
        "agent_id": agent_id,
        "description": request.description,
        "continuation_id": projection.as_ref().map(|projection| projection.continuation_id.to_string()),
        "attempt_id": projection.as_ref().map(|projection| projection.attempt_id.to_string()),
        "checkpoint_id": projection.as_ref().and_then(|projection| projection.checkpoint_id.as_ref().map(ToString::to_string)),
        "resumable": projection.as_ref().is_some_and(|projection| projection.resumable),
        "indeterminate": projection.as_ref().is_some_and(|projection| projection.indeterminate),
    })
    .to_string();
    async_launch_output(
        task_registry,
        &agent_id,
        tool_types::ToolResult::completed(tool_request, output, false),
    )
}

fn async_launch_output(
    task_registry: &TaskRegistry,
    agent_id: &str,
    result: tool_types::ToolResult,
) -> AsyncSubagentLaunchOutput {
    AsyncSubagentLaunchOutput {
        result,
        task: task_registry.summary(agent_id),
    }
}

fn finish_async_launch_failure(
    coordinator: &ChildAgentCoordinator,
    prepared: &PreparedContinuation,
    activity: &ChildAgentActivityEmitter,
    mut error: String,
) -> String {
    let projection = match coordinator.commit_prepared_terminal(
        prepared,
        AgentTerminal::Failed {
            error: error.clone(),
        },
    ) {
        Ok(projection) => Some(projection),
        Err(commit_error) => {
            error.push_str(&format!(
                "\n\nfailed to commit async continuation terminal: {commit_error}"
            ));
            None
        }
    };
    error = append_projection_footer(error, projection.as_ref());
    if let Err(activity_error) = activity.publish_payload(SubagentActivityPayload::Completed {
        status: SurfaceSubagentTerminalStatus::Failed,
        output: None,
        error: Some(DisplayText::new(&error)),
        usage: None,
    }) {
        error.push_str(&format!(
            "\n\nchild activity terminal could not be durably published: {activity_error}"
        ));
    }
    error
}

fn prepare_async_launch_worktree(
    source: Option<&PreparedContinuation>,
    isolation: SubagentIsolation,
    parent_cwd: &Path,
) -> Result<AsyncLaunchWorktree, String> {
    if let Some(source) = source {
        let child_cwd = PathBuf::from(&source.compatibility.effective_cwd);
        if !child_cwd.is_dir() {
            return Err(
                "continuation_incompatible: inherited effective cwd is missing or not a directory"
                    .to_string(),
            );
        }
        let worker = match isolation {
            SubagentIsolation::None => None,
            SubagentIsolation::Worktree => {
                let binding = source.compatibility.worktree.as_ref().ok_or_else(|| {
                    "continuation_incompatible: worktree continuation has no durable binding"
                        .to_string()
                })?;
                Some(AsyncSubagentWorktree {
                    repo_root: PathBuf::from(&binding.repo_root),
                    path: PathBuf::from(&binding.path),
                })
            }
        };
        return Ok(AsyncLaunchWorktree {
            child_cwd,
            worker,
            fresh_guard: None,
        });
    }

    match isolation {
        SubagentIsolation::None => Ok(AsyncLaunchWorktree {
            child_cwd: parent_cwd.to_path_buf(),
            worker: None,
            fresh_guard: None,
        }),
        SubagentIsolation::Worktree => {
            let guard = WorktreeGuard::create(parent_cwd)
                .map_err(|error| format!("failed to create subagent worktree: {error}"))?;
            let child_cwd = guard.path().to_path_buf();
            let worker = Some(AsyncSubagentWorktree {
                repo_root: guard.repo_root().to_path_buf(),
                path: child_cwd.clone(),
            });
            Ok(AsyncLaunchWorktree {
                child_cwd,
                worker,
                fresh_guard: Some(guard),
            })
        }
    }
}

fn wait_for_async_subagent_adoption(
    task_session_id: &str,
    cwd: &Path,
    agent_id: &str,
) -> Result<TaskRegistry, String> {
    let pid = std::process::id();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let registry = TaskRegistry::attach_for_cwd(task_session_id.to_string(), cwd);
        let record = registry.get(agent_id).ok_or_else(|| {
            format!("async subagent task '{agent_id}' disappeared before adoption")
        })?;
        if record.worker_pid == Some(pid) {
            return Ok(registry);
        }
        #[cfg(windows)]
        if record.worker_pid.is_some()
            && ProcessJob::open_named(&crate::tasks::async_worker_job_name(agent_id))
                .and_then(|job| job.contains_process(pid))
                .unwrap_or(false)
        {
            return Ok(registry);
        }
        if matches!(
            record.status,
            orca_core::task_types::TaskStatus::Stopped
                | orca_core::task_types::TaskStatus::Completed
                | orca_core::task_types::TaskStatus::Failed
                | orca_core::task_types::TaskStatus::Cancelled
        ) {
            return Err(format!(
                "async subagent task '{agent_id}' became terminal before worker adoption"
            ));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "async subagent worker was not adopted before the startup deadline"
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn spawn_async_subagent_worker(
    context: AsyncSubagentWorkerSpawnContext<'_>,
) -> Result<(Child, ProcessJob), String> {
    let AsyncSubagentWorkerSpawnContext {
        config,
        cwd,
        child_cwd,
        task_session_id,
        agent_id,
        request,
        child_depth,
        worktree,
        permission_response_public_key,
        child_turn_id,
        activity_start_precommitted,
    } = context;
    let current_exe = std::env::current_exe().map_err(|error| error.to_string())?;
    let request_json = serde_json::to_string(request).map_err(|error| error.to_string())?;
    let api_key = config.api_key.as_deref();
    let mut worker_args = vec![
        "subagent-worker".to_string(),
        "--cwd".to_string(),
        cwd.to_string_lossy().into_owned(),
        "--child-cwd".to_string(),
        child_cwd.to_string_lossy().into_owned(),
        "--provider".to_string(),
        config.provider.as_str().to_string(),
        "--session-id".to_string(),
        task_session_id.to_string(),
        "--agent-id".to_string(),
        agent_id.to_string(),
        "--subagent-depth".to_string(),
        child_depth.to_string(),
        "--request-json".to_string(),
        request_json,
        "--permission-response-public-key".to_string(),
        base64::engine::general_purpose::STANDARD.encode(permission_response_public_key),
        "--child-turn-id".to_string(),
        child_turn_id.to_string(),
    ];
    if activity_start_precommitted {
        worker_args.push("--activity-start-precommitted".to_string());
    }
    if let Some(model) = config.model.as_history_value() {
        worker_args.extend(["--model".to_string(), model.to_string()]);
    }
    if api_key.is_some() {
        worker_args.push("--api-key-stdin".to_string());
    }
    if let Some(base_url) = config.base_url.as_deref() {
        worker_args.extend(["--base-url".to_string(), base_url.to_string()]);
    }
    if let Some(worktree) = worktree {
        worker_args.extend([
            "--worktree-repo-root".to_string(),
            worktree.repo_root.to_string_lossy().into_owned(),
            "--worktree-path".to_string(),
            worktree.path.to_string_lossy().into_owned(),
        ]);
    }
    #[cfg(windows)]
    {
        return spawn_async_subagent_worker_via_runner(
            &current_exe,
            worker_args,
            cwd,
            agent_id,
            api_key,
            CapabilitySet::for_approval_mode(config.approval_mode),
        );
    }
    #[cfg(not(windows))]
    {
        let mut command = ProcessCommand::new(current_exe);
        prepare_async_subagent_worker_command(&mut command, agent_id);
        command
            .current_dir(cwd)
            .stdin(if api_key.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .args(&worker_args)
            .env_remove("ORCA_API_KEY")
            .env_remove("DEEPSEEK_API_KEY");
        let broker = ExecutionBroker::with_backend(
            orca_core::capability::EnforcementState::Advisory,
            "subagent-user-trusted",
        );
        let launched = broker
            .launch_user_trusted(
                command,
                format!("subagent:{agent_id}"),
                cwd,
                CapabilitySet::for_approval_mode(config.approval_mode),
            )
            .map_err(|error| match error {
                LaunchError::Spawn(error) => error.to_string(),
                other => format!("{other:?}"),
            })?;
        let (mut child, process_job) = (launched.child, launched.process_job);
        handoff_async_subagent_worker_api_key(&mut child, api_key)?;
        Ok((child, process_job))
    }
}

#[cfg(windows)]
fn spawn_async_subagent_worker_via_runner(
    current_exe: &Path,
    worker_args: Vec<String>,
    cwd: &Path,
    agent_id: &str,
    api_key: Option<&str>,
    capabilities: CapabilitySet,
) -> Result<(Child, ProcessJob), String> {
    let executable_dir = current_exe
        .parent()
        .ok_or_else(|| "orca executable has no installation directory".to_string())?;
    let runner = executable_dir.join("orca-windows-runner.exe");
    if !runner.is_file() {
        return Err(format!(
            "Windows runner is missing beside the installed Orca executable: {}",
            runner.display()
        ));
    }
    let request = WindowsRunnerLaunchRequest {
        version: WINDOWS_RUNNER_PROTOCOL_VERSION,
        program: current_exe.to_string_lossy().into_owned(),
        args: worker_args,
        cwd: cwd.to_string_lossy().into_owned(),
        env: BTreeMap::new(),
        job_name: Some(crate::tasks::async_worker_job_name(agent_id)),
        forward_stdin: api_key.is_some(),
    };
    let mut command = ProcessCommand::new(runner);
    prepare_async_subagent_worker_command(&mut command, agent_id);
    command
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .env_remove("ORCA_API_KEY")
        .env_remove("DEEPSEEK_API_KEY");
    let broker = ExecutionBroker::with_backend(
        orca_core::capability::EnforcementState::Advisory,
        "subagent-windows-runner",
    );
    let launched = broker
        .launch_user_trusted_named(
            command,
            format!("subagent:{agent_id}"),
            cwd,
            capabilities,
            &crate::tasks::async_worker_job_name(agent_id),
        )
        .map_err(|error| format!("failed to spawn Windows runner: {error:?}"))?;
    let (mut child, process_job) = (launched.child, launched.process_job);
    let result = child
        .stdin
        .take()
        .ok_or_else(|| "Windows runner did not expose request stdin".to_string())
        .and_then(|mut stdin| {
            serde_json::to_writer(&mut stdin, &request)
                .map_err(|error| format!("failed to encode Windows runner request: {error}"))?;
            stdin
                .write_all(b"\n")
                .map_err(|error| format!("failed to terminate Windows runner request: {error}"))?;
            if let Some(api_key) = api_key {
                stdin.write_all(api_key.as_bytes()).map_err(|error| {
                    format!("failed to hand off async subagent credential: {error}")
                })?;
            }
            Ok(())
        });
    if let Err(error) = result {
        orca_tools::process::kill_child_tree(&mut child);
        let _ = child.wait();
        return Err(error);
    }
    Ok((child, process_job))
}

fn prepare_async_subagent_worker_command(
    command: &mut ProcessCommand,
    #[cfg_attr(windows, allow(unused_variables))] agent_id: &str,
) {
    orca_tools::process::prepare_non_interactive_command(command);
    #[cfg(unix)]
    command.arg0(crate::tasks::subagent_worker_process_name(agent_id));
}

#[cfg(not(windows))]
fn handoff_async_subagent_worker_api_key(
    child: &mut Child,
    api_key: Option<&str>,
) -> Result<(), String> {
    let Some(api_key) = api_key else {
        return Ok(());
    };
    let result = child
        .stdin
        .take()
        .ok_or_else(|| "async subagent worker did not expose credential stdin".to_string())
        .and_then(|mut stdin| {
            stdin
                .write_all(api_key.as_bytes())
                .map_err(|error| format!("failed to hand off async subagent credential: {error}"))
        });
    if result.is_err() {
        orca_tools::process::kill_child_tree(child);
        let _ = child.wait();
    }
    result
}

pub(crate) fn usage_totals_if_non_empty(usage: UsageTotals) -> Option<UsageTotals> {
    if usage.total_tokens() == 0 && usage.cache_tokens == 0 && usage.estimated_cost_usd == 0.0 {
        None
    } else {
        Some(usage)
    }
}

fn finish_async_worker_worktree(
    worktree: Option<AsyncSubagentWorktree>,
    owns_worktree: bool,
) -> Option<crate::worktree::WorktreeOutcome> {
    worktree.and_then(|worktree| {
        if owns_worktree {
            WorktreeGuard::finish_existing(worktree.repo_root, worktree.path).ok()
        } else {
            Some(crate::worktree::WorktreeOutcome {
                path: worktree.path,
                preserved: true,
            })
        }
    })
}

fn commit_async_terminal(
    coordinator: &ChildAgentCoordinator,
    lease: &ContinuationLease,
    shared_revision: Option<&Arc<Mutex<ContinuationRevision>>>,
    terminal: AgentTerminal,
) -> Result<ContinuationProjection, crate::agent_continuation::AgentContinuationError> {
    if let Some(shared_revision) = shared_revision {
        let mut revision = shared_revision.lock().map_err(|_| {
            crate::agent_continuation::AgentContinuationError::Persistence {
                message: "async continuation revision lock is poisoned".to_string(),
            }
        })?;
        let projection =
            commit_continuation_write_with_retry(coordinator, lease, *revision, |revision| {
                coordinator.commit_terminal(lease, revision, terminal.clone())
            })?;
        *revision = projection.revision;
        return Ok(projection);
    }
    let revision = coordinator
        .projection(lease.continuation_id.as_str())?
        .revision;
    commit_continuation_write_with_retry(coordinator, lease, revision, |revision| {
        coordinator.commit_terminal(lease, revision, terminal.clone())
    })
}

fn append_projection_footer(
    mut output: String,
    projection: Option<&ContinuationProjection>,
) -> String {
    if let Some(projection) = projection {
        output.push_str("\n\n");
        output.push_str(&continuation_footer(projection));
    }
    output
}

fn panic_payload_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic payload".to_string()
    }
}

pub(crate) fn async_subagent_result_payload(
    output: String,
    task: Option<serde_json::Value>,
) -> String {
    serde_json::json!({
        "output": output,
        "task": task,
    })
    .to_string()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use orca_core::approval_types::{ActionKind, ApprovalMode};
    use orca_core::config::{
        HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, ReasoningEffort, RunConfig,
        ThemeName, ToolConfig, WorkflowConfig,
    };
    use orca_core::model::ModelSelection;
    use orca_core::subagent_config::SubagentConfig;
    use orca_core::tool_types::{ToolName, ToolRequest, ToolStatus};
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn inherited_async_worktree_is_preserved() {
        let outcome = finish_async_worker_worktree(
            Some(AsyncSubagentWorktree {
                repo_root: PathBuf::from("/missing/repo"),
                path: PathBuf::from("/missing/inherited-worktree"),
            }),
            false,
        )
        .expect("worktree outcome");

        assert!(outcome.preserved);
        assert_eq!(outcome.path, PathBuf::from("/missing/inherited-worktree"));
    }

    #[test]
    fn detached_relay_sink_rejects_tampered_activity_before_append() {
        let registry = TaskRegistry::new("relay-sink-validation".to_string());
        let lease = crate::tasks::TaskLease {
            task_id: "missing-task".to_string(),
            owner_id: "worker".to_string(),
            epoch: 1,
            expires_at_ms: i64::MAX,
        };
        let sink = DetachedRelayActivitySink {
            task_registry: registry,
            task_lease: lease,
            task_id: "missing-task".to_string(),
            attempt_id: "attempt".to_string(),
        };
        let mut event = SubagentActivityEvent::new(
            SurfaceTaskId::try_new("missing-task").unwrap(),
            SurfaceSubagentId::try_new("missing-subagent").unwrap(),
            crate::agent_continuation::AgentAttemptId::new(),
            TurnId::new(),
            1,
            SubagentActivityOwner::DetachedTask {
                task_id: SurfaceTaskId::try_new("missing-task").unwrap(),
                task_revision: TaskRevision::try_new(1).unwrap(),
                authority_digest: crate::runtime_surface::Sha256Digest::new([7; 32]),
            },
            SubagentActivityPayload::Started {
                description: DisplayText::new("start"),
                batch_id: "batch-test".to_string(),
                batch_size: 1,
            },
        );
        event.digest = crate::runtime_surface::Sha256Digest::new([0; 32]);

        let error = sink
            .publish(event)
            .expect_err("tampered event must fail closed");
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn process_local_registry_rejects_async_subagent_before_spawn() {
        let cwd = tempfile::tempdir().unwrap();
        let config = async_test_config(cwd.path().to_path_buf());
        let tool_request = ToolRequest {
            id: "ephemeral-async".to_string(),
            name: ToolName::Subagent,
            action: ActionKind::Agent,
            target: Some("inspect later".to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "description": "inspect later",
                    "prompt": "inspect later",
                    "mode": "async"
                })
                .to_string(),
            ),
        };
        let request = subagent::create_subagent_request(&tool_request);
        let registry = TaskRegistry::new("ephemeral-thread".to_string());

        let output = launch_async_subagent(AsyncSubagentLaunchContext {
            config: &config,
            cwd: cwd.path(),
            tool_request: &tool_request,
            request,
            subagent_depth: 0,
            task_registry: &registry,
            root_task_id: None,
            parent_fence: None,
            activity_ingress: None,
        });

        assert_eq!(output.result.status, ToolStatus::Failed);
        assert!(
            output
                .result
                .error
                .as_deref()
                .is_some_and(|error| error.contains("use sync mode"))
        );
        assert!(output.task.is_none());
        assert!(registry.list().is_empty());
        assert!(!cwd.path().join(".orca").exists());
    }

    fn async_test_config(cwd: PathBuf) -> RunConfig {
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
            reasoning_effort: ReasoningEffort::Max,
            api_key: None,
            base_url: None,
            mcp_servers: Vec::new(),
            hooks: Vec::new(),
            external_tools: Vec::new(),
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            active_permission_profile: None,
            permission_profiles: Default::default(),
            runtime_workspace_roots: None,
            permission_rules: Default::default(),
            additional_working_directories: Vec::new(),
            budget: Default::default(),
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

    #[test]
    fn async_subagent_worker_command_hides_key_and_owns_process_group() {
        unsafe extern "C" {
            fn getpgid(pid: i32) -> i32;
        }

        let temp = tempfile::tempdir().unwrap();
        let key_file = temp.path().join("worker-key");
        let sentinel = "orca-secret-sentinel-not-for-argv";
        let agent_id = "task-test-worker";
        let mut command = ProcessCommand::new("sh");
        prepare_async_subagent_worker_command(&mut command, agent_id);
        command
            .env("ORCA_TEST_KEY_FILE", &key_file)
            .stdin(Stdio::piped())
            .arg("-c")
            .arg("cat > \"$ORCA_TEST_KEY_FILE\"; sleep 30");
        let mut child = command.spawn().expect("spawn worker process-group fixture");
        handoff_async_subagent_worker_api_key(&mut child, Some(sentinel))
            .expect("hand off worker credential");
        let pid = child.id() as i32;
        let deadline = Instant::now() + Duration::from_secs(2);
        while std::fs::read_to_string(&key_file).ok().as_deref() != Some(sentinel)
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            key_file.exists(),
            "worker did not receive API key through private stdin"
        );

        let pgid = unsafe { getpgid(pid) };
        let pid_text = pid.to_string();
        let command_line = ProcessCommand::new("/bin/ps")
            .args(["-ww", "-p", pid_text.as_str(), "-o", "command="])
            .output()
            .expect("inspect worker command line");

        assert_eq!(
            pgid, pid,
            "async worker must lead an isolated process group"
        );
        let command_line = String::from_utf8_lossy(&command_line.stdout);
        assert!(
            command_line.starts_with(&crate::tasks::subagent_worker_process_name(agent_id)),
            "async worker must expose its persisted identity in argv0"
        );
        assert!(
            !command_line.contains(sentinel),
            "provider API key must not appear in worker argv"
        );
        assert!(
            !command_line.contains("--api-key"),
            "internal worker must not receive an API key argument"
        );
        assert_eq!(
            std::fs::read_to_string(&key_file).unwrap(),
            sentinel,
            "worker must receive the provider API key through its private stdin"
        );
        orca_tools::process::kill_child_tree(&mut child);
        let _ = child.wait();
    }
}
