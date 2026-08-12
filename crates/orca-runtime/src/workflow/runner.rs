use std::fs;
use std::io;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use orca_core::cancel::CancelToken;
use orca_core::config::{DelegationSnapshot, OutputFormat, RunConfig};
use orca_core::cost_types::UsageTotals;
use orca_core::event_schema::EventFactory;
use orca_core::event_schema::RunStatus;
use orca_core::event_sink::EventSink;
use orca_core::subagent_types::SubagentType;
use orca_core::task_types::{TaskStatus, TaskType, WorkflowPhaseTaskSummary, WorkflowTaskProgress};
use orca_core::workflow_types::{
    WorkflowAgentStatus, WorkflowEvidenceIdentity, WorkflowEvidenceToolEvent, WorkflowInput,
    WorkflowOutput, WorkflowPhaseRecord, WorkflowRunState, WorkflowRunStatus,
    WorkflowTaskLifecycleEvidence, WorkflowTokenBudget,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::agent_child::{
    ChildAgentExecutor, ChildAgentRequest, ChildAgentRuntime, ChildAgentRuntimeContext,
    run_child_agent,
};
use crate::agent_loop::execute_child_agent_loop;
use crate::hooks::HookRunner;
use crate::instructions;
use crate::lifecycle::{
    RuntimeSessionLifecycle, RuntimeTaskKind, RuntimeTaskLifecycle, RuntimeTaskStatus,
};
use crate::memory;
use crate::schema_validation::validate_json_schema_subset;
use crate::tasks::{TaskRecord, TaskRegistry};
use crate::worktree::{WorktreeGuard, WorktreeOutcome};

use super::host::{AgentCall, HostCommand, HostEvent, WorkflowHost, WorkflowHostIpcPaths};
use super::ipc::WorkflowIpcContext;
use super::script::{
    ResolvedWorkflowScript, resolve_workflow_script_to_path, validate_workflow_args,
};
use super::state::{
    WorkflowAgentRecord, WorkflowAgentStatusCounts, WorkflowStateStore, WorkflowWorkerRecord,
    input_hash, workflow_agent_min_hold_ms, workflow_agent_team,
};

const STOP_REQUESTED_ERROR: &str = "__orca_workflow_stop_requested__";
const STOPPED_SUMMARY: &str = "Workflow stopped";

#[derive(Clone, Debug, Default)]
pub struct WorkflowLaunchRequest {
    input: WorkflowInput,
}

impl WorkflowLaunchRequest {
    pub fn from_draft_id(draft_id: String) -> Self {
        Self {
            input: WorkflowInput {
                draft_id: Some(draft_id),
                ..Default::default()
            },
        }
    }

    pub fn from_script_path(script_path: String) -> Self {
        Self {
            input: WorkflowInput {
                script_path: Some(script_path),
                ..Default::default()
            },
        }
    }

    pub fn with_resume_from(mut self, run_id: String) -> Self {
        self.input.resume_from_run_id = Some(run_id);
        self
    }
}

impl From<WorkflowInput> for WorkflowLaunchRequest {
    fn from(input: WorkflowInput) -> Self {
        Self { input }
    }
}

#[derive(Clone, Debug)]
pub struct WorkflowLaunchResult {
    pub task_id: String,
    pub output: WorkflowOutput,
    pub summary: String,
    pub status_line: String,
}

#[derive(Debug)]
pub struct WorkflowBackgroundLaunch {
    pub task_id: String,
    pub run_id: String,
    pub workflow_name: String,
    pub phases: Vec<String>,
    pub output: WorkflowOutput,
    handle: thread::JoinHandle<io::Result<WorkflowLaunchResult>>,
}

impl WorkflowBackgroundLaunch {
    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }

    pub fn join(self) -> thread::Result<io::Result<WorkflowLaunchResult>> {
        self.handle.join()
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        task_id: String,
        run_id: String,
        workflow_name: String,
        phases: Vec<String>,
        handle: thread::JoinHandle<io::Result<WorkflowLaunchResult>>,
    ) -> Self {
        let output = WorkflowOutput {
            status: "async_launched".to_string(),
            task_id: task_id.clone(),
            task_type: Some("local_workflow".to_string()),
            workflow_name: Some(workflow_name.clone()),
            run_id: Some(run_id.clone()),
            summary: Some("Workflow launched".to_string()),
            transcript_dir: None,
            script_path: None,
            session_url: None,
            budget: None,
        };
        Self {
            task_id,
            run_id,
            workflow_name,
            phases,
            output,
            handle,
        }
    }
}

#[derive(Clone, Debug)]
pub struct WorkflowRunner {
    config: RunConfig,
    delegation: DelegationSnapshot,
    tasks: TaskRegistry,
    session_dir: PathBuf,
    state: WorkflowStateStore,
    child_executor: ChildAgentExecutor<SharedEventBuffer>,
}

#[derive(Clone, Debug, Default)]
struct WorkflowExecutionCounters {
    total_agents: u32,
    active_agents: usize,
    max_observed_concurrent_agents: usize,
    settled_tokens: u64,
    reserved_tokens: u64,
    token_budget: Option<u64>,
}

#[derive(Clone, Debug)]
struct PreparedWorkflowRun {
    request: WorkflowLaunchRequest,
    resolved: ResolvedWorkflowScript,
    task_id: String,
    run_id: String,
    transcript_dir: PathBuf,
    state: WorkflowRunState,
    created_at_ms: i64,
}

#[derive(Clone, Debug)]
pub(crate) struct PreparedWorkflowBackgroundLaunch {
    pub(crate) task_id: String,
    pub(crate) run_id: String,
    pub(crate) workflow_name: String,
    pub(crate) workflow_description: String,
    pub(crate) phases: Vec<String>,
    pub(crate) created_at_ms: i64,
    prepared: PreparedWorkflowRun,
}

#[derive(Clone, Debug)]
struct WorkflowChildAgentCallOutput {
    message: String,
    usage: UsageTotals,
    worktree: Option<WorktreeOutcome>,
    tool_events: Vec<WorkflowEvidenceToolEvent>,
    task: Option<WorkflowTaskLifecycleEvidence>,
}

#[derive(Clone, Debug)]
struct WorkflowChildAgentCallError {
    message: String,
    usage: Option<UsageTotals>,
    retryable: bool,
    cancelled: bool,
    tool_events: Vec<WorkflowEvidenceToolEvent>,
    task: Option<WorkflowTaskLifecycleEvidence>,
}

#[derive(Clone, Debug)]
struct WorkflowAgentExecutionPolicy {
    max_agent_retries: u32,
    max_agent_tokens: Option<u64>,
    allowed_tools: Option<Vec<String>>,
    tool_policy_label: Option<String>,
}

impl From<io::Error> for WorkflowChildAgentCallError {
    fn from(error: io::Error) -> Self {
        Self {
            message: error.to_string(),
            usage: None,
            retryable: true,
            cancelled: false,
            tool_events: Vec::new(),
            task: None,
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct SharedEventBuffer {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl SharedEventBuffer {
    fn content(&self) -> Vec<u8> {
        self.bytes
            .lock()
            .map(|bytes| bytes.clone())
            .unwrap_or_default()
    }
}

impl Write for SharedEventBuffer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut bytes = self
            .bytes
            .lock()
            .map_err(|_| io::Error::other("workflow child event buffer poisoned"))?;
        bytes.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
struct WorkflowExecutionGate {
    token_budget: Option<u64>,
    counters: Mutex<WorkflowExecutionCounters>,
    condvar: Condvar,
}

impl WorkflowExecutionGate {
    fn new() -> Self {
        Self {
            token_budget: None,
            counters: Mutex::new(WorkflowExecutionCounters::default()),
            condvar: Condvar::new(),
        }
    }

    fn with_token_budget_and_spent(token_budget: u64, spent: u64) -> Self {
        let token_budget = token_budget.max(1);
        let counters = WorkflowExecutionCounters {
            settled_tokens: spent,
            token_budget: Some(token_budget),
            ..Default::default()
        };
        Self {
            token_budget: Some(token_budget),
            counters: Mutex::new(counters),
            condvar: Condvar::new(),
        }
    }

    fn begin_agent(
        self: &Arc<Self>,
        max_agents_per_run: u32,
        max_concurrent_agents: usize,
        max_agent_tokens: Option<u64>,
    ) -> io::Result<WorkflowAgentPermit> {
        let max_concurrent_agents = max_concurrent_agents.max(1);
        let mut counters = self
            .counters
            .lock()
            .map_err(|_| io::Error::other("workflow execution counters poisoned"))?;
        let reservation_tokens = loop {
            if counters.total_agents >= max_agents_per_run {
                return Err(io::Error::other(format!(
                    "maximum workflow agent count {max_agents_per_run} exceeded"
                )));
            }
            if counters.active_agents >= max_concurrent_agents {
                counters = self
                    .condvar
                    .wait(counters)
                    .map_err(|_| io::Error::other("workflow execution counters poisoned"))?;
                continue;
            }

            let Some(token_budget) = self.token_budget else {
                break 0;
            };
            let settled_remaining = token_budget.saturating_sub(counters.settled_tokens);
            if settled_remaining == 0 {
                return Err(io::Error::other(format!(
                    "workflow run {token_budget} token budget exhausted after {} tokens",
                    counters.settled_tokens
                )));
            }
            let available = settled_remaining.saturating_sub(counters.reserved_tokens);
            let requested = max_agent_tokens
                .unwrap_or(settled_remaining)
                .max(1)
                .min(settled_remaining);
            if requested <= available {
                break requested;
            }

            counters = self
                .condvar
                .wait(counters)
                .map_err(|_| io::Error::other("workflow execution counters poisoned"))?;
        };
        counters.total_agents += 1;
        counters.active_agents += 1;
        counters.reserved_tokens = counters.reserved_tokens.saturating_add(reservation_tokens);
        counters.max_observed_concurrent_agents = counters
            .max_observed_concurrent_agents
            .max(counters.active_agents);
        Ok(WorkflowAgentPermit {
            gate: Arc::clone(self),
            reservation_tokens,
            settled: false,
        })
    }

    fn finish_agent(&self) {
        if let Ok(mut counters) = self.counters.lock() {
            if counters.active_agents > 0 {
                counters.active_agents -= 1;
            }
            self.condvar.notify_one();
        }
    }

    fn snapshot(&self) -> io::Result<WorkflowExecutionCounters> {
        self.counters
            .lock()
            .map(|guard| guard.clone())
            .map_err(|_| io::Error::other("workflow execution counters poisoned"))
    }

    fn settle_agent(&self, reservation_tokens: u64, tokens: u64) -> io::Result<()> {
        let mut counters = self
            .counters
            .lock()
            .map_err(|_| io::Error::other("workflow execution counters poisoned"))?;
        counters.reserved_tokens = counters.reserved_tokens.saturating_sub(reservation_tokens);
        counters.settled_tokens = counters.settled_tokens.saturating_add(tokens);
        self.condvar.notify_all();
        Ok(())
    }

    fn is_exhausted(&self) -> io::Result<bool> {
        let counters = self
            .counters
            .lock()
            .map_err(|_| io::Error::other("workflow execution counters poisoned"))?;
        Ok(self
            .token_budget
            .is_some_and(|total| counters.settled_tokens >= total))
    }

    fn budget(&self) -> io::Result<Option<WorkflowTokenBudget>> {
        let counters = self
            .counters
            .lock()
            .map_err(|_| io::Error::other("workflow execution counters poisoned"))?;
        Ok(self
            .token_budget
            .map(|total| WorkflowTokenBudget::from_total_and_spent(total, counters.settled_tokens)))
    }
}

#[derive(Debug)]
struct WorkflowAgentPermit {
    gate: Arc<WorkflowExecutionGate>,
    reservation_tokens: u64,
    settled: bool,
}

impl WorkflowAgentPermit {
    fn budget_cap(&self) -> Option<u64> {
        (self.reservation_tokens > 0).then_some(self.reservation_tokens)
    }

    fn settle_usage(&mut self, tokens: u64) -> io::Result<()> {
        if !self.settled {
            self.gate.settle_agent(self.reservation_tokens, tokens)?;
            self.settled = true;
        }
        Ok(())
    }
}

impl Drop for WorkflowAgentPermit {
    fn drop(&mut self) {
        if !self.settled {
            let _ = self.gate.settle_agent(self.reservation_tokens, 0);
        }
        self.gate.finish_agent();
    }
}

impl WorkflowRunner {
    pub fn new(config: RunConfig, tasks: TaskRegistry, session_dir: PathBuf) -> Self {
        let state = WorkflowStateStore::new(session_dir.join("workflow-runs"));
        let delegation = DelegationSnapshot::from_config(&config);
        Self {
            config,
            delegation,
            tasks,
            session_dir,
            state,
            child_executor: execute_child_agent_loop,
        }
    }

    pub(crate) fn with_child_executor(
        mut self,
        child_executor: ChildAgentExecutor<SharedEventBuffer>,
    ) -> Self {
        self.child_executor = child_executor;
        self
    }

    pub fn launch(&self, request: WorkflowLaunchRequest) -> io::Result<WorkflowLaunchResult> {
        let prepared = self.prepare_launch(request)?;
        self.activate_prepared_run(&prepared)?;
        self.execute_prepared(prepared)
    }

    pub fn resume(&self, request: WorkflowLaunchRequest) -> io::Result<WorkflowLaunchResult> {
        let prepared = self.prepare_launch(request)?;
        self.activate_prepared_run(&prepared)?;
        self.execute_prepared(prepared)
    }

    pub fn launch_background(
        &self,
        request: WorkflowLaunchRequest,
    ) -> io::Result<WorkflowBackgroundLaunch> {
        let prepared = self.prepare_background(request)?;
        self.activate_background(prepared)
    }

    pub(crate) fn prepare_background(
        &self,
        request: WorkflowLaunchRequest,
    ) -> io::Result<PreparedWorkflowBackgroundLaunch> {
        let prepared = self.prepare_launch(request)?;
        let task_id = prepared.task_id.clone();
        let run_id = prepared.run_id.clone();
        let workflow_name = prepared.resolved.meta.name.clone();
        let workflow_description = prepared.resolved.meta.description.clone();
        let phases = prepared.resolved.meta.phases.clone();
        let created_at_ms = prepared.created_at_ms;
        Ok(PreparedWorkflowBackgroundLaunch {
            task_id,
            run_id,
            workflow_name,
            workflow_description,
            phases,
            created_at_ms,
            prepared,
        })
    }

    pub(crate) fn activate_background(
        &self,
        launch: PreparedWorkflowBackgroundLaunch,
    ) -> io::Result<WorkflowBackgroundLaunch> {
        let PreparedWorkflowBackgroundLaunch {
            task_id,
            run_id,
            workflow_name,
            workflow_description: _,
            phases,
            created_at_ms: _,
            prepared,
        } = launch;
        let resumed_spent =
            self.resumed_spent_tokens(prepared.request.input.resume_from_run_id.as_deref())?;
        self.activate_prepared_run(&prepared)?;
        let output = WorkflowOutput {
            status: "async_launched".to_string(),
            task_id: task_id.clone(),
            task_type: Some("local_workflow".to_string()),
            workflow_name: Some(workflow_name.clone()),
            run_id: Some(run_id.clone()),
            summary: Some("Workflow launched".to_string()),
            transcript_dir: Some(prepared.transcript_dir.display().to_string()),
            script_path: Some(prepared.resolved.persisted_path.display().to_string()),
            session_url: None,
            budget: prepared
                .request
                .input
                .token_budget
                .map(|total| WorkflowTokenBudget::from_total_and_spent(total, resumed_spent)),
        };
        self.state.write_worker_record(
            &run_id,
            &WorkflowWorkerRecord {
                pid: std::process::id(),
                active: true,
                started_at_ms: now_ms(),
                completed_at_ms: None,
            },
        )?;
        let runner = self.clone();
        let handle = thread::spawn(move || runner.execute_prepared(prepared));

        Ok(WorkflowBackgroundLaunch {
            task_id,
            run_id,
            workflow_name,
            phases,
            output,
            handle,
        })
    }

    pub(crate) fn abort_prepared_background(
        &self,
        launch: PreparedWorkflowBackgroundLaunch,
        message: String,
    ) {
        if let Ok(mut state) = self.state.load_run(&launch.run_id) {
            state.status = WorkflowRunStatus::Failed;
            state.error = Some(message.clone());
            let _ = self.state.write_state(&state);
        }
        let _ = self.tasks.fail(&launch.task_id, message);
    }

    pub(crate) fn reconcile_durable_terminal_outcome(
        &self,
        task_id: &str,
        run_id: &str,
    ) -> io::Result<Option<TaskRecord>> {
        let Some(mut task) = self.tasks.get(task_id) else {
            return Ok(None);
        };
        if task.task_type != TaskType::Workflow || task.workflow_run_id.as_deref() != Some(run_id) {
            return Ok(None);
        }
        let interrupted_task = task.status == TaskStatus::Failed
            && task.error.as_deref().is_some_and(|error| {
                error.contains(
                    "interrupted before completion; async task execution is process-local",
                )
            });
        let mut state = match self.state.load_run(run_id) {
            Ok(state) => state,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if self.state.worker_path(run_id).exists() {
                    self.state.mark_worker_exited(run_id)?;
                }
                return if !interrupted_task
                    && matches!(
                        task.status,
                        TaskStatus::Completed
                            | TaskStatus::Failed
                            | TaskStatus::Stopped
                            | TaskStatus::Cancelled
                    ) {
                    Ok(Some(task))
                } else {
                    Ok(None)
                };
            }
            Err(error) => return Err(error),
        };
        if state.run_id != run_id
            || state.task_id != task_id
            || state.session_id != self.tasks.session_id()
        {
            return Ok(None);
        }

        let state_terminal = matches!(
            state.status,
            WorkflowRunStatus::Completed
                | WorkflowRunStatus::Failed
                | WorkflowRunStatus::Stopped
                | WorkflowRunStatus::Cancelled
        );
        if state_terminal && interrupted_task {
            match state.status {
                WorkflowRunStatus::Completed => self
                    .tasks
                    .complete(
                        task_id,
                        state
                            .final_summary
                            .clone()
                            .unwrap_or_else(|| "Workflow completed".to_string()),
                    )
                    .map_err(io::Error::other)?,
                WorkflowRunStatus::Failed => self
                    .tasks
                    .fail(
                        task_id,
                        state
                            .error
                            .clone()
                            .unwrap_or_else(|| "Workflow failed".to_string()),
                    )
                    .map_err(io::Error::other)?,
                WorkflowRunStatus::Stopped | WorkflowRunStatus::Cancelled => self
                    .tasks
                    .stop(
                        task_id,
                        state
                            .final_summary
                            .clone()
                            .unwrap_or_else(|| STOPPED_SUMMARY.to_string()),
                    )
                    .map_err(io::Error::other)?,
                _ => unreachable!("state terminal was checked"),
            }
            task = self
                .tasks
                .get(task_id)
                .ok_or_else(|| io::Error::other("repaired workflow task disappeared"))?;
        } else {
            match task.status {
                TaskStatus::Completed => {
                    state.status = WorkflowRunStatus::Completed;
                    state.final_summary = task.result.clone();
                    state.error = None;
                }
                TaskStatus::Failed if !interrupted_task => {
                    state.status = WorkflowRunStatus::Failed;
                    state.final_summary = None;
                    state.error = task.error.clone();
                }
                TaskStatus::Stopped => {
                    state.status = WorkflowRunStatus::Stopped;
                    state.final_summary = task.result.clone();
                    state.error = None;
                }
                TaskStatus::Cancelled => {
                    state.status = WorkflowRunStatus::Cancelled;
                    state.final_summary = task.result.clone();
                    state.error = None;
                }
                _ => return Ok(None),
            }
            self.state.write_state(&state)?;
        }
        if self.state.worker_path(run_id).exists() {
            self.state.mark_worker_exited(run_id)?;
        }
        Ok(Some(task))
    }

    pub(crate) fn settle_runtime_restart_without_durable_outcome(
        &self,
        task_id: &str,
        run_id: &str,
    ) -> io::Result<()> {
        let mut state = match self.state.load_run(run_id) {
            Ok(state) => state,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        if state.run_id != run_id
            || state.task_id != task_id
            || state.session_id != self.tasks.session_id()
        {
            return Ok(());
        }
        state.status = WorkflowRunStatus::Failed;
        state.final_summary = None;
        state.error = Some("Workflow interrupted by runtime restart".to_string());
        self.state.write_state(&state)?;
        if self.state.worker_path(run_id).exists() {
            self.state.mark_worker_exited(run_id)?;
        }
        Ok(())
    }

    fn prepare_launch(&self, request: WorkflowLaunchRequest) -> io::Result<PreparedWorkflowRun> {
        let mut request = request;
        let cwd = self.config.cwd.clone().unwrap_or(std::env::current_dir()?);
        fs::create_dir_all(&self.session_dir)?;

        if request.input.token_budget.is_none()
            && let Some(resume_from_run_id) = request.input.resume_from_run_id.as_deref()
        {
            request.input.token_budget = self
                .state
                .load_launch_input(resume_from_run_id)
                .ok()
                .and_then(|input| input.token_budget);
        }
        if request.input.token_budget == Some(0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "tokenBudget must be at least 1",
            ));
        }

        let run_id = format!("workflow-run-{}", uuid::Uuid::new_v4());
        let persisted_script_path = self.state.run_dir(&run_id).join("script.js");
        let resolved_input = self.resolve_launch_input(&request.input)?;
        let resolved =
            resolve_workflow_script_to_path(&resolved_input, &cwd, &persisted_script_path)?;
        let normalized_args =
            validate_workflow_args(resolved_input.args.clone(), &resolved.args_schema)?;
        request.input.args = Some(normalized_args);
        let task_id = format!("task-{}", uuid::Uuid::new_v4());
        let state = WorkflowRunState {
            run_id: run_id.clone(),
            task_id: task_id.clone(),
            session_id: self.tasks.session_id().to_string(),
            cwd: cwd.display().to_string(),
            workflow_name: resolved.meta.name.clone(),
            meta: resolved.meta.clone(),
            script_digest: resolved.script_digest.clone(),
            args_digest: digest_value(request.input.args.as_ref().unwrap_or(&Value::Null)),
            status: WorkflowRunStatus::Queued,
            phases: Vec::new(),
            total_agent_count: 0,
            final_summary: None,
            error: None,
        };

        let transcript_dir = self.state.transcript_dir(&run_id);

        Ok(PreparedWorkflowRun {
            request,
            resolved,
            task_id,
            run_id,
            transcript_dir,
            state,
            created_at_ms: now_ms(),
        })
    }

    fn activate_prepared_run(&self, prepared: &PreparedWorkflowRun) -> io::Result<()> {
        self.tasks
            .activate_prepared_workflow(
                prepared.task_id.clone(),
                prepared.run_id.clone(),
                prepared.resolved.meta.name.clone(),
                prepared.resolved.meta.description.clone(),
                prepared.resolved.meta.phases.len(),
                prepared.created_at_ms,
            )
            .map_err(io::Error::other)?;
        self.state.create_run(&prepared.state)?;
        self.state
            .write_launch_input(&prepared.run_id, &prepared.request.input)?;
        self.tasks
            .update_workflow_artifacts(
                &prepared.task_id,
                prepared.resolved.persisted_path.display().to_string(),
                prepared.request.input.clone(),
            )
            .map_err(io::Error::other)?;
        self.tasks
            .mark_running(&prepared.task_id)
            .map_err(io::Error::other)?;
        let mut state = prepared.state.clone();
        state.status = WorkflowRunStatus::Running;
        self.state.write_state(&state)
    }

    fn resolve_launch_input(&self, input: &WorkflowInput) -> io::Result<WorkflowInput> {
        let Some(draft_id) = input
            .draft_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        else {
            return Ok(input.clone());
        };

        if input.script.is_some() || input.script_path.is_some() || input.name.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "workflow draftId cannot be combined with script, scriptPath, or name",
            ));
        }

        let script_path = self
            .session_dir
            .join("workflow-drafts")
            .join(draft_id)
            .join("script.js");
        if !script_path.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("workflow draft `{draft_id}` not found"),
            ));
        }

        Ok(WorkflowInput {
            draft_id: None,
            script_path: Some(script_path.display().to_string()),
            args: input.args.clone(),
            token_budget: input.token_budget,
            resume_from_run_id: input.resume_from_run_id.clone(),
            restart_phase: input.restart_phase.clone(),
            ..Default::default()
        })
    }

    fn execute_prepared(&self, prepared: PreparedWorkflowRun) -> io::Result<WorkflowLaunchResult> {
        let PreparedWorkflowRun {
            request,
            resolved,
            task_id,
            run_id,
            transcript_dir,
            state: _,
            created_at_ms: _,
        } = prepared;
        let mut state = self.state.load_run(&run_id)?;
        let args = request.input.args.clone().unwrap_or(Value::Null);
        let resume_from = request.input.resume_from_run_id.clone();
        let restart_phase = request.input.restart_phase.clone();
        let cached_agents = Arc::new(AtomicU32::new(0));
        let mut failed_error = None;
        let mut completed_result = None;
        let resumed_spent = self.resumed_spent_tokens(resume_from.as_deref())?;
        let gate = Arc::new(match request.input.token_budget {
            Some(token_budget) => {
                WorkflowExecutionGate::with_token_budget_and_spent(token_budget, resumed_spent)
            }
            None => WorkflowExecutionGate::new(),
        });
        let ipc_paths = WorkflowHostIpcPaths {
            mailbox_path: self.state.mailbox_path(&run_id),
            task_lists_path: self.state.task_lists_path(&run_id),
        };
        let workflow_ipc = WorkflowIpcContext::new_durable(
            ipc_paths.mailbox_path.clone(),
            ipc_paths.task_lists_path.clone(),
        )?;
        let workflow_limits = self.config.workflows.clone();
        let workflow_cancel = CancelToken::new();

        if self.state.stop_requested(&run_id)? {
            return self.finish_stopped_run(
                state,
                resolved.meta.name,
                resolved.persisted_path,
                transcript_dir,
                task_id,
                run_id,
                gate.snapshot()?,
            );
        }

        let mut phase_agent_baselines = std::collections::HashMap::new();
        let mut agent_events_seen = 0u32;
        match WorkflowHost::run_with_agent_event_control_and_ipc_paths(
            &resolved.persisted_path,
            args,
            &ipc_paths,
            workflow_limits.max_concurrent_agents,
            |call| {
                self.answer_agent_call(
                    &run_id,
                    &task_id,
                    resume_from.as_deref(),
                    restart_phase.as_deref(),
                    &transcript_dir,
                    call,
                    Arc::clone(&cached_agents),
                    &gate,
                    &workflow_ipc,
                    &workflow_limits,
                    &workflow_cancel,
                )
            },
            |event| {
                apply_host_event_to_state(
                    event,
                    &mut state,
                    &mut phase_agent_baselines,
                    &mut agent_events_seen,
                    &mut completed_result,
                    &mut failed_error,
                );
                self.state.write_state(&state)?;
                self.refresh_task_progress(&task_id, &state)
            },
            || {
                let stopped =
                    self.state.stop_requested(&run_id)? || self.tasks.is_cancelled(&task_id);
                if stopped {
                    workflow_cancel.cancel();
                }
                Ok(stopped)
            },
            || workflow_cancel.cancel(),
        ) {
            Ok(()) => {}
            Err(error) => {
                let message = error.to_string();
                if error.kind() == io::ErrorKind::Interrupted
                    && (self.state.stop_requested(&run_id)? || self.tasks.is_cancelled(&task_id))
                {
                    return self.finish_stopped_run(
                        state,
                        resolved.meta.name,
                        resolved.persisted_path,
                        transcript_dir,
                        task_id,
                        run_id,
                        gate.snapshot()?,
                    );
                }
                let counts = self.state.agent_status_counts(&run_id)?;
                let counters = gate.snapshot()?;
                state.total_agent_count = counters.total_agents.max(terminal_agent_count(&counts));
                state.status = WorkflowRunStatus::Failed;
                state.error = Some(message.clone());
                self.state.write_state(&state)?;
                self.write_evidence_for_state(&state, Some(&counters))?;
                self.tasks
                    .fail(&task_id, message.clone())
                    .map_err(io::Error::other)?;
                let _ = self.state.mark_worker_exited(&run_id);
                return Err(error);
            }
        }

        let counts = self.state.agent_status_counts(&run_id)?;
        state.total_agent_count = gate
            .snapshot()?
            .total_agents
            .max(terminal_agent_count(&counts));
        if let Some(error) = failed_error {
            if is_stop_requested_error(&error) {
                return self.finish_stopped_run(
                    state,
                    resolved.meta.name,
                    resolved.persisted_path,
                    transcript_dir,
                    task_id,
                    run_id,
                    gate.snapshot()?,
                );
            }
            state.status = WorkflowRunStatus::Failed;
            state.error = Some(error.clone());
            self.state.write_state(&state)?;
            let counters = gate.snapshot()?;
            self.write_evidence_for_state(&state, Some(&counters))?;
            self.tasks
                .fail(&task_id, error.clone())
                .map_err(io::Error::other)?;
            let _ = self.state.mark_worker_exited(&run_id);
            return Err(io::Error::other(error));
        }

        if self.state.stop_requested(&run_id)? {
            return self.finish_stopped_run(
                state,
                resolved.meta.name,
                resolved.persisted_path,
                transcript_dir,
                task_id,
                run_id,
                gate.snapshot()?,
            );
        }

        let result = completed_result.unwrap_or_default();
        let cached_agents = cached_agents.load(Ordering::Relaxed);
        let cache_summary = if cached_agents == 1 {
            "cached 1 agent".to_string()
        } else {
            format!("cached {cached_agents} agents")
        };
        let summary = if cached_agents > 0 {
            format!("{result} ({cache_summary})")
        } else {
            result.clone()
        };

        state.status = WorkflowRunStatus::Completed;
        state.final_summary = Some(summary.clone());
        self.state.write_state(&state)?;
        let counters = gate.snapshot()?;
        self.write_evidence_for_state(&state, Some(&counters))?;
        let status_line = workflow_status_line(
            &state,
            &counts,
            &counters,
            self.config.workflows.max_concurrent_agents,
            gate.budget()?,
        );
        self.refresh_task_progress(&task_id, &state)?;
        self.tasks
            .complete(&task_id, result.clone())
            .map_err(io::Error::other)?;
        let _ = self.state.mark_worker_exited(&run_id);

        Ok(WorkflowLaunchResult {
            task_id: task_id.clone(),
            output: WorkflowOutput {
                status: "completed".to_string(),
                task_id,
                task_type: Some(task_type_name(TaskType::Workflow).to_string()),
                workflow_name: Some(resolved.meta.name),
                run_id: Some(run_id),
                summary: Some(summary.clone()),
                transcript_dir: Some(transcript_dir.display().to_string()),
                script_path: Some(resolved.persisted_path.display().to_string()),
                session_url: None,
                budget: gate.budget()?,
            },
            summary,
            status_line,
        })
    }

    fn resumed_spent_tokens(&self, resume_from: Option<&str>) -> io::Result<u64> {
        resume_from
            .map(|run_id| {
                self.state.agent_summaries(run_id).map(|agents| {
                    agents
                        .into_iter()
                        .filter_map(|agent| agent.usage)
                        .map(UsageTotals::total_tokens)
                        .sum()
                })
            })
            .transpose()
            .map(|spent| spent.unwrap_or_default())
    }

    fn answer_agent_call(
        &self,
        run_id: &str,
        task_id: &str,
        resume_from: Option<&str>,
        restart_phase: Option<&str>,
        transcript_dir: &std::path::Path,
        call: AgentCall,
        cached_agents: Arc<AtomicU32>,
        gate: &Arc<WorkflowExecutionGate>,
        workflow_ipc: &WorkflowIpcContext,
        workflow_limits: &orca_core::config::WorkflowConfig,
        workflow_cancel: &CancelToken,
    ) -> io::Result<HostCommand> {
        if self.workflow_stop_requested(run_id, task_id, workflow_cancel)? {
            return Ok(HostCommand::AgentError {
                call_id: call.call_id,
                error: STOP_REQUESTED_ERROR.to_string(),
            });
        }
        if self.wait_while_paused(run_id, task_id, workflow_cancel)? {
            return Ok(HostCommand::AgentError {
                call_id: call.call_id,
                error: STOP_REQUESTED_ERROR.to_string(),
            });
        }
        let hash = input_hash(&call.prompt, &call.opts);
        if let Some(resume_run_id) = resume_from
            && !call_path_matches_phase(&call.call_path, restart_phase)
        {
            if let Some(cached_value) =
                self.state
                    .find_cached_agent_value(resume_run_id, &call.call_path, &hash)
            {
                cached_agents.fetch_add(1, Ordering::Relaxed);
                let normalized_cached_value = normalize_cached_agent_result(&call, cached_value);
                if let Err(error_message) =
                    validate_workflow_agent_schema(&call, &normalized_cached_value)
                {
                    let transcript_path =
                        write_agent_transcript(transcript_dir, &call, &error_message, true)?;
                    self.state.record_agent_completed(
                        run_id,
                        WorkflowAgentRecord {
                            call_id: call.call_id.clone(),
                            call_path: call.call_path.clone(),
                            prompt: call.prompt.clone(),
                            opts: call.opts.clone(),
                            team: workflow_agent_team(&call.opts),
                            input_hash: hash,
                            status: WorkflowAgentStatus::Failed,
                            attempt: 1,
                            max_attempts: 1,
                            previous_errors: Vec::new(),
                            output: Some(normalized_cached_value),
                            error: Some(error_message.clone()),
                            transcript_path: Some(transcript_path.display().to_string()),
                            started_at_ms: Some(now_ms()),
                            completed_at_ms: Some(now_ms()),
                            usage: None,
                            task: None,
                            tool_events: Vec::new(),
                        },
                    )?;
                    if let Ok(state) = self.state.load_run(run_id) {
                        let _ = self.refresh_task_progress(task_id, &state);
                    }
                    return Ok(HostCommand::AgentError {
                        call_id: call.call_id,
                        error: error_message,
                    });
                }
                let output = result_to_summary(&normalized_cached_value);
                let transcript_path = write_agent_transcript(transcript_dir, &call, &output, true)?;
                self.state.record_agent_completed(
                    run_id,
                    WorkflowAgentRecord {
                        call_id: call.call_id.clone(),
                        call_path: call.call_path.clone(),
                        prompt: call.prompt.clone(),
                        opts: call.opts.clone(),
                        team: workflow_agent_team(&call.opts),
                        input_hash: hash,
                        status: WorkflowAgentStatus::Cached,
                        attempt: 1,
                        max_attempts: 1,
                        previous_errors: Vec::new(),
                        output: Some(normalized_cached_value.clone()),
                        error: None,
                        transcript_path: Some(transcript_path.display().to_string()),
                        started_at_ms: Some(now_ms()),
                        completed_at_ms: Some(now_ms()),
                        usage: None,
                        task: None,
                        tool_events: Vec::new(),
                    },
                )?;
                if let Ok(state) = self.state.load_run(run_id) {
                    let _ = self.refresh_task_progress(task_id, &state);
                }
                return Ok(HostCommand::AgentResult {
                    call_id: call.call_id,
                    result: normalized_cached_value,
                });
            }
        }

        let execution_policy = workflow_agent_execution_policy(&call, workflow_limits);
        let max_attempts = execution_policy.max_agent_retries.saturating_add(1).max(1);
        let mut previous_errors = Vec::new();
        for attempt in 1..=max_attempts {
            if self.workflow_stop_requested(run_id, task_id, workflow_cancel)? {
                return Ok(HostCommand::AgentError {
                    call_id: call.call_id,
                    error: STOP_REQUESTED_ERROR.to_string(),
                });
            }
            let mut permit = match gate.begin_agent(
                workflow_limits.max_agents_per_run,
                workflow_limits.max_concurrent_agents,
                execution_policy.max_agent_tokens,
            ) {
                Ok(permit) => permit,
                Err(error) => {
                    return Ok(HostCommand::AgentError {
                        call_id: call.call_id,
                        error: error.to_string(),
                    });
                }
            };
            if self.workflow_stop_requested(run_id, task_id, workflow_cancel)? {
                self.record_cancelled_agent(
                    run_id,
                    task_id,
                    transcript_dir,
                    &call,
                    &hash,
                    attempt,
                    max_attempts,
                    &previous_errors,
                    now_ms(),
                )?;
                return Ok(HostCommand::AgentError {
                    call_id: call.call_id,
                    error: STOP_REQUESTED_ERROR.to_string(),
                });
            }
            let started_at_ms = now_ms();
            self.state.record_agent_completed(
                run_id,
                WorkflowAgentRecord {
                    call_id: call.call_id.clone(),
                    call_path: call.call_path.clone(),
                    prompt: call.prompt.clone(),
                    opts: call.opts.clone(),
                    team: workflow_agent_team(&call.opts),
                    input_hash: hash.clone(),
                    status: WorkflowAgentStatus::Running,
                    attempt,
                    max_attempts,
                    previous_errors: previous_errors.clone(),
                    output: None,
                    error: None,
                    transcript_path: None,
                    started_at_ms: Some(started_at_ms),
                    completed_at_ms: None,
                    usage: None,
                    task: None,
                    tool_events: Vec::new(),
                },
            )?;
            if let Ok(state) = self.state.load_run(run_id) {
                let _ = self.refresh_task_progress(task_id, &state);
            }
            if let Some(min_hold_ms) = workflow_agent_min_hold_ms(&call.opts)
                && self.wait_for_agent_delay(
                    run_id,
                    task_id,
                    workflow_cancel,
                    std::time::Duration::from_millis(min_hold_ms),
                )?
            {
                self.record_cancelled_agent(
                    run_id,
                    task_id,
                    transcript_dir,
                    &call,
                    &hash,
                    attempt,
                    max_attempts,
                    &previous_errors,
                    started_at_ms,
                )?;
                return Ok(HostCommand::AgentError {
                    call_id: call.call_id,
                    error: STOP_REQUESTED_ERROR.to_string(),
                });
            }

            let mut child_execution_policy = execution_policy.clone();
            if let Some(budget_cap) = permit.budget_cap() {
                child_execution_policy.max_agent_tokens = Some(budget_cap);
            }
            match self.run_child_agent_call(
                &call,
                workflow_ipc,
                &child_execution_policy,
                workflow_cancel,
            ) {
                Ok(child_output) => {
                    permit.settle_usage(child_output.usage.total_tokens())?;
                    if gate.is_exhausted()? {
                        workflow_cancel.cancel();
                    }
                    let completed_at_ms = now_ms();
                    let child_task = child_output.task.clone();
                    let mut output = child_agent_output(&call.prompt, &child_output.message);
                    append_worktree_outcome(&mut output, child_output.worktree.as_ref());
                    let transcript_path =
                        write_agent_transcript(transcript_dir, &call, &output, false)?;
                    let result = workflow_agent_result(&call, Value::String(output.clone()), false);
                    if let Err(error_message) = validate_workflow_agent_schema(&call, &result) {
                        self.state.record_agent_completed(
                            run_id,
                            WorkflowAgentRecord {
                                call_id: call.call_id.clone(),
                                call_path: call.call_path.clone(),
                                prompt: call.prompt.clone(),
                                opts: call.opts.clone(),
                                team: workflow_agent_team(&call.opts),
                                input_hash: hash.clone(),
                                status: WorkflowAgentStatus::Failed,
                                attempt,
                                max_attempts,
                                previous_errors: previous_errors.clone(),
                                output: Some(result),
                                error: Some(error_message.clone()),
                                transcript_path: Some(transcript_path.display().to_string()),
                                started_at_ms: Some(started_at_ms),
                                completed_at_ms: Some(completed_at_ms),
                                usage: Some(child_output.usage),
                                task: child_task,
                                tool_events: child_output.tool_events,
                            },
                        )?;
                        if let Ok(state) = self.state.load_run(run_id) {
                            let _ = self.refresh_task_progress(task_id, &state);
                        }

                        return Ok(HostCommand::AgentError {
                            call_id: call.call_id,
                            error: error_message,
                        });
                    }
                    self.state.record_agent_completed(
                        run_id,
                        WorkflowAgentRecord {
                            call_id: call.call_id.clone(),
                            call_path: call.call_path.clone(),
                            prompt: call.prompt.clone(),
                            opts: call.opts.clone(),
                            team: workflow_agent_team(&call.opts),
                            input_hash: hash.clone(),
                            status: WorkflowAgentStatus::Completed,
                            attempt,
                            max_attempts,
                            previous_errors: previous_errors.clone(),
                            output: Some(result.clone()),
                            error: None,
                            transcript_path: Some(transcript_path.display().to_string()),
                            started_at_ms: Some(started_at_ms),
                            completed_at_ms: Some(completed_at_ms),
                            usage: Some(child_output.usage),
                            task: child_task,
                            tool_events: child_output.tool_events,
                        },
                    )?;
                    if let Ok(state) = self.state.load_run(run_id) {
                        let _ = self.refresh_task_progress(task_id, &state);
                    }

                    return Ok(HostCommand::AgentResult {
                        call_id: call.call_id,
                        result,
                    });
                }
                Err(error) => {
                    let completed_at_ms = now_ms();
                    let WorkflowChildAgentCallError {
                        message: error_message,
                        usage,
                        retryable,
                        cancelled,
                        tool_events,
                        task,
                    } = error;
                    permit
                        .settle_usage(usage.map(UsageTotals::total_tokens).unwrap_or_default())?;
                    if gate.is_exhausted()? {
                        workflow_cancel.cancel();
                    }
                    drop(permit);
                    let transcript_path =
                        write_agent_transcript(transcript_dir, &call, &error_message, false)?;
                    self.state.record_agent_completed(
                        run_id,
                        WorkflowAgentRecord {
                            call_id: call.call_id.clone(),
                            call_path: call.call_path.clone(),
                            prompt: call.prompt.clone(),
                            opts: call.opts.clone(),
                            team: workflow_agent_team(&call.opts),
                            input_hash: hash.clone(),
                            status: if cancelled {
                                WorkflowAgentStatus::Cancelled
                            } else {
                                WorkflowAgentStatus::Failed
                            },
                            attempt,
                            max_attempts,
                            previous_errors: previous_errors.clone(),
                            output: None,
                            error: Some(error_message.clone()),
                            transcript_path: Some(transcript_path.display().to_string()),
                            started_at_ms: Some(started_at_ms),
                            completed_at_ms: Some(completed_at_ms),
                            usage,
                            task,
                            tool_events,
                        },
                    )?;
                    if let Ok(state) = self.state.load_run(run_id) {
                        let _ = self.refresh_task_progress(task_id, &state);
                    }

                    if attempt < max_attempts && retryable && !cancelled {
                        previous_errors.push(error_message);
                        continue;
                    }

                    return Ok(HostCommand::AgentError {
                        call_id: call.call_id,
                        error: error_message,
                    });
                }
            }
        }

        Ok(HostCommand::AgentError {
            call_id: call.call_id,
            error: "workflow child agent exhausted retry attempts".to_string(),
        })
    }

    fn workflow_stop_requested(
        &self,
        run_id: &str,
        task_id: &str,
        workflow_cancel: &CancelToken,
    ) -> io::Result<bool> {
        if workflow_cancel.is_cancelled() || self.tasks.is_cancelled(task_id) {
            return Ok(true);
        }
        self.state.stop_requested(run_id)
    }

    #[allow(clippy::too_many_arguments)]
    fn record_cancelled_agent(
        &self,
        run_id: &str,
        task_id: &str,
        transcript_dir: &std::path::Path,
        call: &AgentCall,
        input_hash: &str,
        attempt: u32,
        max_attempts: u32,
        previous_errors: &[String],
        started_at_ms: i64,
    ) -> io::Result<()> {
        let completed_at_ms = now_ms();
        let transcript_path =
            write_agent_transcript(transcript_dir, call, STOP_REQUESTED_ERROR, false)?;
        self.state.record_agent_completed(
            run_id,
            WorkflowAgentRecord {
                call_id: call.call_id.clone(),
                call_path: call.call_path.clone(),
                prompt: call.prompt.clone(),
                opts: call.opts.clone(),
                team: workflow_agent_team(&call.opts),
                input_hash: input_hash.to_string(),
                status: WorkflowAgentStatus::Cancelled,
                attempt,
                max_attempts,
                previous_errors: previous_errors.to_vec(),
                output: None,
                error: Some(STOP_REQUESTED_ERROR.to_string()),
                transcript_path: Some(transcript_path.display().to_string()),
                started_at_ms: Some(started_at_ms),
                completed_at_ms: Some(completed_at_ms),
                usage: None,
                task: None,
                tool_events: Vec::new(),
            },
        )?;
        if let Ok(state) = self.state.load_run(run_id) {
            let _ = self.refresh_task_progress(task_id, &state);
        }
        Ok(())
    }

    fn wait_for_agent_delay(
        &self,
        run_id: &str,
        task_id: &str,
        workflow_cancel: &CancelToken,
        delay: std::time::Duration,
    ) -> io::Result<bool> {
        let started = std::time::Instant::now();
        loop {
            if self.wait_while_paused(run_id, task_id, workflow_cancel)? {
                return Ok(true);
            }
            if self.workflow_stop_requested(run_id, task_id, workflow_cancel)? {
                return Ok(true);
            }
            let Some(remaining) = delay.checked_sub(started.elapsed()) else {
                return Ok(false);
            };
            thread::sleep(remaining.min(std::time::Duration::from_millis(50)));
        }
    }

    fn wait_while_paused(
        &self,
        run_id: &str,
        task_id: &str,
        workflow_cancel: &CancelToken,
    ) -> io::Result<bool> {
        let mut paused = false;
        while self.state.pause_requested(run_id)? {
            if self.workflow_stop_requested(run_id, task_id, workflow_cancel)? {
                return Ok(true);
            }
            let mut state = self.state.load_run(run_id)?;
            if state.status != WorkflowRunStatus::Paused {
                state.status = WorkflowRunStatus::Paused;
                self.state.write_state(&state)?;
                self.refresh_task_progress(task_id, &state)?;
            }
            let _ = self.tasks.request_pause(task_id);
            paused = true;
            thread::sleep(std::time::Duration::from_millis(50));
        }

        if paused {
            let mut state = self.state.load_run(run_id)?;
            if state.status == WorkflowRunStatus::Paused {
                state.status = WorkflowRunStatus::Running;
                self.state.write_state(&state)?;
                self.refresh_task_progress(task_id, &state)?;
            }
            let _ = self.tasks.request_resume(task_id);
        }
        Ok(false)
    }

    fn run_child_agent_call(
        &self,
        call: &AgentCall,
        workflow_ipc: &WorkflowIpcContext,
        execution_policy: &WorkflowAgentExecutionPolicy,
        cancel: &CancelToken,
    ) -> Result<WorkflowChildAgentCallOutput, WorkflowChildAgentCallError> {
        let cwd = self
            .config
            .cwd
            .as_deref()
            .unwrap_or(self.session_dir.as_path());
        let worktree_guard = if workflow_agent_uses_worktree(&call.opts) {
            Some(WorktreeGuard::create(cwd)?)
        } else {
            None
        };
        let child_cwd = worktree_guard
            .as_ref()
            .map(|guard| guard.path())
            .unwrap_or(cwd);
        let mut events = EventFactory::new(format!("workflow-child-{}", call.call_id));
        let event_buffer = SharedEventBuffer::default();
        let mut sink = EventSink::new(event_buffer.clone(), OutputFormat::Jsonl);
        let instructions = instructions::load_for_cwd_or_default(cwd);
        let memory = memory::load_for_cwd(cwd);
        let (workflow_child_config, mcp_registry) =
            Self::workflow_child_runtime_parts(&self.config, &self.delegation);
        let hooks = HookRunner::new(self.config.hooks.clone());
        let child_request = ChildAgentRequest {
            prompt: call.prompt.clone(),
            subagent_type: SubagentType::General,
            model: None,
            depth: 1,
            emit_deltas: true,
            allowed_tools: execution_policy.allowed_tools.clone(),
            tool_policy_label: execution_policy.tool_policy_label.clone(),
            workflow_ipc: Some(workflow_ipc.for_sender(call.call_path.clone())),
        };
        let mut lifecycle =
            RuntimeSessionLifecycle::new(format!("workflow-child-{}", call.call_id));
        lifecycle.start_task(RuntimeTaskKind::Subagent);
        let mut runtime = ChildAgentRuntime::new(ChildAgentRuntimeContext {
            cwd: child_cwd,
            events: &mut events,
            sink: &mut sink,
            instructions: &instructions,
            memory: &memory,
            mcp_registry: &mcp_registry,
            hooks: &hooks,
            cancel,
            lifecycle: Some(&mut lifecycle),
            task_registry: None,
            root_task_id: None,
            executor: self.child_executor,
        });
        let (result, child_cost_tracker) =
            run_child_agent(&workflow_child_config, &child_request, &mut runtime);
        drop(runtime);
        let task = lifecycle
            .finish_task(result.status)
            .map(workflow_task_lifecycle_evidence)
            .or_else(|| {
                lifecycle
                    .active_task()
                    .map(workflow_task_lifecycle_evidence)
            });
        let usage = child_cost_tracker.totals();
        let tool_events = parse_child_tool_events(&event_buffer.content());
        let worktree = worktree_guard.map(WorktreeGuard::finish).transpose()?;

        match result.status {
            RunStatus::Success => {
                if let Some(max_agent_tokens) = execution_policy.max_agent_tokens {
                    let total_tokens = usage.total_tokens();
                    if total_tokens > max_agent_tokens {
                        let mut error = format!(
                            "{total_tokens} tokens exceeded per-agent token budget {max_agent_tokens}"
                        );
                        append_worktree_outcome(&mut error, worktree.as_ref());
                        return Err(WorkflowChildAgentCallError {
                            message: error,
                            usage: Some(usage),
                            retryable: false,
                            cancelled: false,
                            tool_events,
                            task,
                        });
                    }
                }
                Ok(WorkflowChildAgentCallOutput {
                    message: result.final_message.unwrap_or_default(),
                    usage,
                    worktree,
                    tool_events,
                    task,
                })
            }
            _ => {
                let cancelled = result.status == RunStatus::Cancelled;
                let mut error = result
                    .error
                    .or(result.final_message)
                    .unwrap_or_else(|| "workflow child agent failed".to_string());
                append_worktree_outcome(&mut error, worktree.as_ref());
                let retryable = !cancelled && is_retryable_child_agent_error(&error);
                Err(WorkflowChildAgentCallError {
                    message: error,
                    usage: Some(usage),
                    retryable,
                    cancelled,
                    tool_events,
                    task,
                })
            }
        }
    }

    fn workflow_child_config(config: &RunConfig, delegation: &DelegationSnapshot) -> RunConfig {
        let mut workflow_child_config = config.clone();
        delegation.apply_to(&mut workflow_child_config, None);
        workflow_child_config.output_format = OutputFormat::Jsonl;
        workflow_child_config
    }

    fn workflow_child_runtime_parts(
        config: &RunConfig,
        delegation: &DelegationSnapshot,
    ) -> (RunConfig, orca_mcp::McpRegistry) {
        let workflow_child_config = Self::workflow_child_config(config, delegation);
        let mcp_registry = orca_mcp::initialize_registry(&workflow_child_config.mcp_servers);
        (workflow_child_config, mcp_registry)
    }

    fn finish_stopped_run(
        &self,
        mut state: WorkflowRunState,
        workflow_name: String,
        script_path: PathBuf,
        transcript_dir: PathBuf,
        task_id: String,
        run_id: String,
        counters: WorkflowExecutionCounters,
    ) -> io::Result<WorkflowLaunchResult> {
        state.status = WorkflowRunStatus::Stopped;
        state.total_agent_count = counters.total_agents;
        state.final_summary = Some(STOPPED_SUMMARY.to_string());
        state.error = None;
        self.state.write_state(&state)?;
        self.write_evidence_for_state(&state, Some(&counters))?;
        self.tasks
            .stop(&task_id, STOPPED_SUMMARY.to_string())
            .map_err(io::Error::other)?;
        let _ = self.state.mark_worker_exited(&run_id);
        let counts = self.state.agent_status_counts(&run_id)?;

        Ok(WorkflowLaunchResult {
            task_id: task_id.clone(),
            output: WorkflowOutput {
                status: "stopped".to_string(),
                task_id,
                task_type: Some(task_type_name(TaskType::Workflow).to_string()),
                workflow_name: Some(workflow_name),
                run_id: Some(run_id),
                summary: Some(STOPPED_SUMMARY.to_string()),
                transcript_dir: Some(transcript_dir.display().to_string()),
                script_path: Some(script_path.display().to_string()),
                session_url: None,
                budget: counters.token_budget.map(|total| {
                    WorkflowTokenBudget::from_total_and_spent(total, counters.settled_tokens)
                }),
            },
            summary: STOPPED_SUMMARY.to_string(),
            status_line: workflow_status_line(
                &state,
                &counts,
                &counters,
                self.config.workflows.max_concurrent_agents,
                counters.token_budget.map(|total| {
                    WorkflowTokenBudget::from_total_and_spent(total, counters.settled_tokens)
                }),
            ),
        })
    }

    fn refresh_task_progress(&self, task_id: &str, state: &WorkflowRunState) -> io::Result<()> {
        let counts = self.state.agent_status_counts(&state.run_id)?;
        let terminal_agents = terminal_agent_count(&counts);
        let running_agents = state.total_agent_count.saturating_sub(terminal_agents);
        let progress = WorkflowTaskProgress {
            total_agents: state.total_agent_count,
            running_agents,
            completed_agents: counts.completed.saturating_add(counts.cached),
            failed_agents: counts.failed.saturating_add(counts.cancelled),
            completed_phases: state
                .phases
                .iter()
                .filter(|phase| phase.status == WorkflowRunStatus::Completed)
                .count(),
            running_phases: state
                .phases
                .iter()
                .filter(|phase| phase.status == WorkflowRunStatus::Running)
                .count(),
            failed_phases: state
                .phases
                .iter()
                .filter(|phase| {
                    matches!(
                        phase.status,
                        WorkflowRunStatus::Failed
                            | WorkflowRunStatus::Cancelled
                            | WorkflowRunStatus::Stopped
                    )
                })
                .count(),
        };
        self.tasks
            .update_workflow_progress(task_id, progress)
            .map_err(io::Error::other)?;
        self.tasks
            .update_workflow_phases(task_id, workflow_phase_summaries(state))
            .map_err(io::Error::other)?;
        self.tasks
            .update_workflow_agents(task_id, self.state.agent_summaries(&state.run_id)?)
            .map_err(io::Error::other)?;
        self.tasks
            .update_workflow_result_summary(
                task_id,
                state.final_summary.clone(),
                workflow_failure_count(state, &self.state.agent_status_counts(&state.run_id)?),
            )
            .map_err(io::Error::other)
    }

    fn write_evidence_for_state(
        &self,
        state: &WorkflowRunState,
        counters: Option<&WorkflowExecutionCounters>,
    ) -> io::Result<()> {
        let identity = WorkflowEvidenceIdentity {
            app_version: self.config.app_version.clone(),
            binary_path: std::env::current_exe()
                .ok()
                .map(|path| path.display().to_string()),
            generated_at_ms: now_ms(),
        };
        let mut bundle = self.state.build_evidence_bundle(state, identity)?;
        bundle.max_configured_concurrent_agents =
            self.config.workflows.max_concurrent_agents as u32;
        bundle.max_observed_concurrent_agents = counters
            .map(|counters| counters.max_observed_concurrent_agents as u32)
            .unwrap_or_default();
        self.state.write_evidence_bundle(&bundle)
    }
}

fn terminal_agent_count(counts: &WorkflowAgentStatusCounts) -> u32 {
    counts
        .completed
        .saturating_add(counts.failed)
        .saturating_add(counts.cancelled)
        .saturating_add(counts.cached)
}

fn workflow_failure_count(state: &WorkflowRunState, counts: &WorkflowAgentStatusCounts) -> u32 {
    let phase_failures = state
        .phases
        .iter()
        .filter(|phase| phase.status == WorkflowRunStatus::Failed)
        .count() as u32;
    let workflow_failure = u32::from(state.status == WorkflowRunStatus::Failed);
    counts
        .failed
        .saturating_add(counts.cancelled)
        .saturating_add(phase_failures)
        .saturating_add(workflow_failure)
}

fn workflow_status_line(
    state: &WorkflowRunState,
    counts: &WorkflowAgentStatusCounts,
    counters: &WorkflowExecutionCounters,
    max_configured_concurrent_agents: usize,
    budget: Option<WorkflowTokenBudget>,
) -> String {
    let completed_agents = counts.completed.saturating_add(counts.cached);
    let failed_agents = counts.failed.saturating_add(counts.cancelled);
    let running_agents = state
        .total_agent_count
        .saturating_sub(terminal_agent_count(counts));
    let completed_phases = state
        .phases
        .iter()
        .filter(|phase| phase.status == WorkflowRunStatus::Completed)
        .count();
    let failed_phases = state
        .phases
        .iter()
        .filter(|phase| {
            matches!(
                phase.status,
                WorkflowRunStatus::Failed
                    | WorkflowRunStatus::Cancelled
                    | WorkflowRunStatus::Stopped
            )
        })
        .count();
    let fallback_phases = state
        .phases
        .iter()
        .filter(|phase| phase.fallback.is_some())
        .count();
    let budget_line = budget
        .map(|budget| {
            let warning = u128::from(budget.spent) * 100 >= u128::from(budget.total) * 80
                && budget.remaining > 0;
            format!(
                "\nBudget: {} spent / {} tokens ({} remaining){}",
                budget.spent,
                budget.total,
                budget.remaining,
                if warning { " · warning" } else { "" }
            )
        })
        .unwrap_or_default();
    format!(
        "Workflow {}\nAgents: {} completed, {} failed, {} running\nPhases: {} completed, {} failed, {} with fallback\nMax observed concurrency: {} / {}\nInspect: /workflows -> {}{}",
        workflow_run_status_label(state.status),
        completed_agents,
        failed_agents,
        running_agents,
        completed_phases,
        failed_phases,
        fallback_phases,
        counters.max_observed_concurrent_agents,
        max_configured_concurrent_agents,
        state.run_id,
        budget_line
    )
}

fn workflow_run_status_label(status: WorkflowRunStatus) -> &'static str {
    match status {
        WorkflowRunStatus::Queued => "queued",
        WorkflowRunStatus::Running => "running",
        WorkflowRunStatus::Paused => "paused",
        WorkflowRunStatus::Stopping => "stopping",
        WorkflowRunStatus::Stopped => "stopped",
        WorkflowRunStatus::Completed => "completed",
        WorkflowRunStatus::Failed => "failed",
        WorkflowRunStatus::Cancelled => "cancelled",
        WorkflowRunStatus::AsyncLaunched => "async_launched",
    }
}

fn call_path_matches_phase(call_path: &str, restart_phase: Option<&str>) -> bool {
    let Some(restart_phase) = restart_phase
        .map(str::trim)
        .filter(|phase| !phase.is_empty())
    else {
        return false;
    };
    let Some((phase, _)) = call_path.split_once(':') else {
        return call_path == restart_phase;
    };
    phase == restart_phase || phase.strip_prefix("phases.") == Some(restart_phase)
}

fn workflow_task_lifecycle_evidence(task: &RuntimeTaskLifecycle) -> WorkflowTaskLifecycleEvidence {
    WorkflowTaskLifecycleEvidence {
        task_id: task.id().to_string(),
        kind: match task.kind() {
            RuntimeTaskKind::Agent => "agent",
            RuntimeTaskKind::Workflow => "workflow",
            RuntimeTaskKind::Subagent => "subagent",
            RuntimeTaskKind::Shell => "shell",
        }
        .to_string(),
        status: match task.status() {
            RuntimeTaskStatus::Running => "running",
            RuntimeTaskStatus::Succeeded => "succeeded",
            RuntimeTaskStatus::Failed => "failed",
            RuntimeTaskStatus::Cancelled => "cancelled",
            RuntimeTaskStatus::ApprovalRequired => "approval_required",
            RuntimeTaskStatus::BudgetExhausted => "budget_exhausted",
        }
        .to_string(),
        turn: task.current_turn(),
    }
}

fn parse_child_tool_events(bytes: &[u8]) -> Vec<WorkflowEvidenceToolEvent> {
    let content = String::from_utf8_lossy(bytes);
    let mut events_by_id = std::collections::BTreeMap::<String, WorkflowEvidenceToolEvent>::new();
    let mut anonymous_events = Vec::new();
    for line in content.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(event_type) = event.get("type").and_then(Value::as_str) else {
            continue;
        };
        if event_type != "tool.call.requested" && event_type != "tool.call.completed" {
            continue;
        }
        let payload = event.get("payload").unwrap_or(&Value::Null);
        let name = payload
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("tool")
            .to_string();
        let id = payload
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let tool_event = WorkflowEvidenceToolEvent {
            id: id.clone(),
            name: name.clone(),
            status: payload
                .get("status")
                .and_then(Value::as_str)
                .map(str::to_string),
            target: payload
                .get("target")
                .and_then(Value::as_str)
                .map(str::to_string),
            error: payload
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string),
            is_mcp: name.starts_with("mcp__"),
        };
        if let Some(id) = id {
            events_by_id
                .entry(id)
                .and_modify(|existing| merge_tool_event(existing, &tool_event))
                .or_insert(tool_event);
        } else {
            anonymous_events.push(tool_event);
        }
    }
    events_by_id.into_values().chain(anonymous_events).collect()
}

fn merge_tool_event(existing: &mut WorkflowEvidenceToolEvent, update: &WorkflowEvidenceToolEvent) {
    if existing.status.is_none() {
        existing.status = update.status.clone();
    }
    if existing.target.is_none() {
        existing.target = update.target.clone();
    }
    if existing.error.is_none() {
        existing.error = update.error.clone();
    }
    existing.is_mcp |= update.is_mcp;
}

fn workflow_phase_summaries(state: &WorkflowRunState) -> Vec<WorkflowPhaseTaskSummary> {
    state
        .phases
        .iter()
        .map(|phase| WorkflowPhaseTaskSummary {
            name: phase.name.clone(),
            status: phase.status,
            agent_count: phase.agent_count,
            error: phase.error.clone(),
            fallback: phase.fallback.clone(),
        })
        .collect()
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn workflow_agent_uses_worktree(opts: &Value) -> bool {
    opts.get("isolation").and_then(Value::as_str) == Some("worktree")
}

fn workflow_agent_execution_policy(
    call: &AgentCall,
    workflow_limits: &orca_core::config::WorkflowConfig,
) -> WorkflowAgentExecutionPolicy {
    let mut policy = WorkflowAgentExecutionPolicy {
        max_agent_retries: workflow_limits.max_agent_retries,
        max_agent_tokens: workflow_limits.max_agent_tokens,
        allowed_tools: None,
        tool_policy_label: None,
    };

    if let Some(team) = workflow_agent_team(&call.opts)
        && let Some(team_policy) = workflow_limits.teams.get(&team)
    {
        policy.tool_policy_label = Some(format!("workflow team '{team}'"));
        if let Some(max_agent_retries) = team_policy.max_agent_retries {
            policy.max_agent_retries = max_agent_retries;
        }
        if let Some(max_agent_tokens) = team_policy.max_agent_tokens {
            policy.max_agent_tokens = Some(max_agent_tokens);
        }
        if let Some(allowed_tools) = &team_policy.allowed_tools {
            policy.allowed_tools = Some(allowed_tools.clone());
        }
    }

    policy
}

fn validate_workflow_agent_schema(call: &AgentCall, result: &Value) -> Result<(), String> {
    let Some(schema) = call.opts.get("schema") else {
        return Ok(());
    };
    validate_json_schema_subset(schema, result, "$").map_err(|error| {
        format!(
            "workflow agent output schema validation failed for {}: {error}",
            call.call_id
        )
    })
}

fn is_retryable_child_agent_error(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    !(error.contains("disallows tool")
        || error.contains("tool")
        || error.contains("command")
        || error.contains("mcp__")
        || error.contains("mcp")
        || error.contains("token budget")
        || error.contains("tokens exceeded")
        || error.contains("schema validation"))
}

fn append_worktree_outcome(output: &mut String, outcome: Option<&WorktreeOutcome>) {
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

fn apply_host_event_to_state(
    event: &HostEvent,
    state: &mut WorkflowRunState,
    phase_agent_baselines: &mut std::collections::HashMap<String, u32>,
    agent_events_seen: &mut u32,
    completed_result: &mut Option<String>,
    failed_error: &mut Option<String>,
) {
    match event {
        HostEvent::PhaseStarted { name } => {
            phase_agent_baselines.insert(name.clone(), *agent_events_seen);
            state.phases.push(WorkflowPhaseRecord {
                name: name.clone(),
                status: WorkflowRunStatus::Running,
                started_at_ms: Some(now_ms()),
                completed_at_ms: None,
                agent_count: 0,
                error: None,
                fallback: None,
            });
        }
        HostEvent::PhaseCompleted { name } => {
            if let Some(phase) = state
                .phases
                .iter_mut()
                .rev()
                .find(|phase| phase.name == *name)
            {
                phase.status = WorkflowRunStatus::Completed;
                phase.completed_at_ms = Some(now_ms());
                let baseline = phase_agent_baselines.get(name).copied().unwrap_or(0);
                phase.agent_count = agent_events_seen.saturating_sub(baseline);
            }
        }
        HostEvent::PhaseFailed {
            name,
            error,
            fallback,
        } => {
            if let Some(phase) = state
                .phases
                .iter_mut()
                .rev()
                .find(|phase| phase.name == *name)
            {
                phase.status = WorkflowRunStatus::Failed;
                phase.completed_at_ms = Some(now_ms());
                let baseline = phase_agent_baselines.get(name).copied().unwrap_or(0);
                phase.agent_count = agent_events_seen.saturating_sub(baseline);
                phase.error = Some(error.clone());
                phase.fallback = fallback.clone();
            }
        }
        HostEvent::AgentCall { phase, .. } => {
            *agent_events_seen += 1;
            state.total_agent_count = *agent_events_seen;
            if let Some(phase_name) = phase
                && let Some(phase) = state
                    .phases
                    .iter_mut()
                    .rev()
                    .find(|phase| phase.name == *phase_name)
            {
                let baseline = phase_agent_baselines.get(phase_name).copied().unwrap_or(0);
                phase.agent_count = agent_events_seen.saturating_sub(baseline);
            }
        }
        HostEvent::WorkflowCompleted { result } => {
            *completed_result = Some(result_to_summary(result));
        }
        HostEvent::WorkflowFailed { error } => {
            if is_stop_requested_error(error) {
                finalize_open_phases_as_stopped(
                    &mut state.phases,
                    phase_agent_baselines,
                    *agent_events_seen,
                );
            } else {
                finalize_open_phases_as_failed(
                    &mut state.phases,
                    phase_agent_baselines,
                    *agent_events_seen,
                );
            }
            *failed_error = Some(error.clone());
        }
    }
}

fn write_agent_transcript(
    transcript_dir: &std::path::Path,
    call: &AgentCall,
    output: &str,
    cached: bool,
) -> io::Result<PathBuf> {
    fs::create_dir_all(transcript_dir)?;
    let path = transcript_dir.join(format!("{}.json", call.call_id));
    let content = serde_json::json!({
        "callId": call.call_id,
        "callPath": call.call_path,
        "phase": call.phase,
        "prompt": call.prompt,
        "opts": call.opts,
        "cached": cached,
        "result": output,
    });
    let encoded = serde_json::to_string_pretty(&content)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    fs::write(&path, encoded)?;
    Ok(path)
}

fn result_to_summary(result: &Value) -> String {
    match result {
        Value::String(value) => value.clone(),
        Value::Object(map) => summary_from_object_phases(map.get("phases"))
            .or_else(|| summary_from_dsl_phases(map.get("phases")))
            .or_else(|| {
                map.get("result").and_then(|value| match value {
                    Value::String(text) => Some(text.clone()),
                    other => Some(result_to_summary(other)),
                })
            })
            .or_else(|| {
                map.get("prompt")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .unwrap_or_else(|| result.to_string()),
        Value::Null => String::new(),
        value => value.to_string(),
    }
}

fn summary_from_object_phases(phases: Option<&Value>) -> Option<String> {
    let phases = phases?.as_object()?;
    let mut parts = Vec::new();
    for (name, phase_value) in phases {
        if let Some(obj) = phase_value.as_object() {
            if let Some(error) = obj.get("error").and_then(Value::as_str) {
                parts.push(format!("{name}: {error}"));
            }
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("; "))
}

fn summary_from_dsl_phases(phases: Option<&Value>) -> Option<String> {
    let phases = phases?.as_array()?;
    for phase in phases.iter().rev() {
        let Some(results) = phase.get("results").and_then(Value::as_array) else {
            continue;
        };
        for result in results.iter().rev() {
            let summary = result_to_summary(result);
            if !summary.trim().is_empty() {
                return Some(summary);
            }
        }
    }
    None
}

fn finalize_open_phases_as_failed(
    phases: &mut [WorkflowPhaseRecord],
    phase_agent_baselines: &std::collections::HashMap<String, u32>,
    agent_events_seen: u32,
) {
    let completed_at_ms = now_ms();
    for phase in phases.iter_mut().filter(|phase| {
        phase.status == WorkflowRunStatus::Running && phase.completed_at_ms.is_none()
    }) {
        phase.status = WorkflowRunStatus::Failed;
        phase.completed_at_ms = Some(completed_at_ms);
        let baseline = phase_agent_baselines.get(&phase.name).copied().unwrap_or(0);
        phase.agent_count = agent_events_seen.saturating_sub(baseline);
    }
}

fn finalize_open_phases_as_stopped(
    phases: &mut [WorkflowPhaseRecord],
    phase_agent_baselines: &std::collections::HashMap<String, u32>,
    agent_events_seen: u32,
) {
    let completed_at_ms = now_ms();
    for phase in phases.iter_mut().filter(|phase| {
        phase.status == WorkflowRunStatus::Running && phase.completed_at_ms.is_none()
    }) {
        phase.status = WorkflowRunStatus::Stopped;
        phase.completed_at_ms = Some(completed_at_ms);
        let baseline = phase_agent_baselines.get(&phase.name).copied().unwrap_or(0);
        phase.agent_count = agent_events_seen.saturating_sub(baseline);
    }
}

fn is_stop_requested_error(error: &str) -> bool {
    error.contains(STOP_REQUESTED_ERROR)
}

fn workflow_agent_result(call: &AgentCall, result: Value, cached: bool) -> Value {
    serde_json::json!({
        "callId": call.call_id,
        "callPath": call.call_path,
        "phase": call.phase,
        "prompt": call.prompt,
        "opts": call.opts,
        "cached": cached,
        "result": result,
    })
}

fn normalize_cached_agent_result(call: &AgentCall, cached_value: Value) -> Value {
    match cached_value {
        Value::Object(map)
            if map.contains_key("callId")
                && map.contains_key("callPath")
                && map.contains_key("prompt")
                && map.contains_key("cached") =>
        {
            Value::Object(map)
        }
        Value::Object(map) => Value::Object(map),
        other => workflow_agent_result(call, other, true),
    }
}

fn child_agent_output(prompt: &str, final_message: &str) -> String {
    if final_message.contains(prompt) {
        final_message.to_string()
    } else if final_message.trim().is_empty() {
        prompt.to_string()
    } else {
        format!("{prompt}\n\n{final_message}")
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;
    use crate::agent_child::ChildAgentResult;
    use crate::cost::CostTracker;
    use orca_core::approval_rules::PermissionRule;
    use orca_core::approval_types::{ApprovalMode, Decision};
    use orca_core::config::{
        ActivePermissionProfile, AdditionalWorkingDirectory, HistoryMode, OutputFormat,
        ProviderKind, ToolConfig, WorkflowConfig,
    };
    use orca_core::mcp_types::McpServerConfig;
    use orca_core::model::ModelSelection;
    use tempfile::tempdir;

    #[test]
    fn workflow_child_config_preserves_parent_approval_mode() {
        let mut config = test_run_config();
        config.approval_mode = ApprovalMode::Suggest;
        let delegation = DelegationSnapshot::from_config(&config);

        let child_config = WorkflowRunner::workflow_child_config(&config, &delegation);
        assert_eq!(child_config.approval_mode, ApprovalMode::Suggest);
    }

    #[test]
    fn workflow_child_config_preserves_parent_plan_mode() {
        let mut config = test_run_config();
        config.approval_mode = ApprovalMode::Plan;
        let delegation = DelegationSnapshot::from_config(&config);

        let child_config = WorkflowRunner::workflow_child_config(&config, &delegation);
        assert_eq!(child_config.approval_mode, ApprovalMode::Plan);
    }

    #[test]
    fn workflow_child_config_applies_immutable_delegation_policy_snapshot() {
        let mut config = test_run_config();
        config.approval_mode = ApprovalMode::Plan;
        config.active_permission_profile =
            Some(ActivePermissionProfile::new("strict", None::<String>));
        config.runtime_workspace_roots = Some(vec![PathBuf::from("/captured")]);
        config
            .permission_rules
            .rules
            .push(PermissionRule::new("bash", "cargo *", Decision::Allow));
        config
            .additional_working_directories
            .push(AdditionalWorkingDirectory::new(
                "/captured-extra",
                "workflow",
            ));
        config.model = ModelSelection::parse(Some("deepseek-v4-flash".to_string())).unwrap();
        let delegation = DelegationSnapshot::from_config(&config);

        config.approval_mode = ApprovalMode::FullAuto;
        config.active_permission_profile = None;
        config.runtime_workspace_roots = Some(vec![PathBuf::from("/mutated")]);
        config.permission_rules = Default::default();
        config.additional_working_directories.clear();
        config.model = ModelSelection::parse(Some("deepseek-v4-pro".to_string())).unwrap();

        let child_config = WorkflowRunner::workflow_child_config(&config, &delegation);

        assert_eq!(child_config.approval_mode, ApprovalMode::Plan);
        assert_eq!(
            child_config.active_permission_profile,
            Some(ActivePermissionProfile::new("strict", None::<String>))
        );
        assert_eq!(
            child_config.runtime_workspace_roots,
            Some(vec![PathBuf::from("/captured")])
        );
        assert_eq!(child_config.permission_rules.rules.len(), 1);
        assert_eq!(child_config.additional_working_directories.len(), 1);
        assert_eq!(child_config.model.as_deref(), Some("deepseek-v4-flash"));
        assert_eq!(child_config.output_format, OutputFormat::Jsonl);
    }

    #[test]
    fn workflow_child_config_disables_interactive_text_prompts() {
        let mut config = test_run_config();
        config.output_format = OutputFormat::Text;
        let delegation = DelegationSnapshot::from_config(&config);

        let child_config = WorkflowRunner::workflow_child_config(&config, &delegation);
        assert_eq!(child_config.output_format, OutputFormat::Jsonl);
    }

    #[test]
    fn workflow_child_registry_uses_configured_mcp_servers() {
        let mut config = test_run_config();
        config.mcp_servers = vec![McpServerConfig {
            name: String::new(),
            ..Default::default()
        }];
        let delegation = DelegationSnapshot::from_config(&config);

        let (_, registry) = WorkflowRunner::workflow_child_runtime_parts(&config, &delegation);
        let registry_error_count = registry.errors().len();
        assert!(
            registry_error_count > 0,
            "workflow child runtime should use initialized MCP registry from config"
        );
    }

    #[test]
    fn workflow_child_runtime_parts_make_child_noninteractive_and_initialize_registry() {
        let mut config = test_run_config();
        config.approval_mode = ApprovalMode::Suggest;
        config.output_format = OutputFormat::Text;
        config.mcp_servers = vec![McpServerConfig {
            name: String::new(),
            ..Default::default()
        }];
        let delegation = DelegationSnapshot::from_config(&config);

        let (child_config, registry) =
            WorkflowRunner::workflow_child_runtime_parts(&config, &delegation);
        assert_eq!(child_config.approval_mode, ApprovalMode::Suggest);
        assert_eq!(child_config.output_format, OutputFormat::Jsonl);
        assert!(
            !registry.errors().is_empty(),
            "workflow child runtime should initialize MCP registry from configured servers"
        );
    }

    #[test]
    fn prepared_background_workflow_has_no_live_worker_before_activation() {
        let temp = tempdir().unwrap();
        let workflow_dir = temp.path().join(".orca").join("workflows");
        fs::create_dir_all(&workflow_dir).unwrap();
        fs::write(
            workflow_dir.join("prepare-only.js"),
            "export const meta = { name: 'prepare-only', description: 'prepare only', phases: ['main'] };\nexport default 'done';",
        )
        .unwrap();
        let mut config = test_run_config();
        config.cwd = Some(temp.path().to_path_buf());
        let tasks = TaskRegistry::new("workflow-prepare-only".to_string());
        let runner =
            WorkflowRunner::new(config, tasks.clone(), temp.path().join("workflow-session"));

        let prepared = runner
            .prepare_background(WorkflowLaunchRequest::from(WorkflowInput {
                script_path: Some(workflow_dir.join("prepare-only.js").display().to_string()),
                ..Default::default()
            }))
            .expect("prepare background workflow");

        assert!(!runner.state.worker_path(&prepared.run_id).exists());
        assert!(tasks.get(&prepared.task_id).is_none());
        runner.abort_prepared_background(prepared.clone(), "surface commit failed".to_string());
        assert!(!runner.state.worker_path(&prepared.run_id).exists());
        assert!(tasks.get(&prepared.task_id).is_none());
    }

    #[test]
    fn resumed_background_workflow_reports_prior_token_spend() {
        let temp = tempdir().unwrap();
        let script_path = temp.path().join("resume-budget.js");
        fs::write(
            &script_path,
            "export const meta = { name: 'resume-budget', description: 'resume budget', phases: [] };\nexport default 'done';",
        )
        .unwrap();
        let mut config = test_run_config();
        config.cwd = Some(temp.path().to_path_buf());
        let tasks = TaskRegistry::new("workflow-resume-budget".to_string());
        let runner = WorkflowRunner::new(config, tasks, temp.path().join("workflow-session"));
        let source = runner
            .prepare_background(WorkflowLaunchRequest::from(WorkflowInput {
                script_path: Some(script_path.display().to_string()),
                token_budget: Some(100),
                ..Default::default()
            }))
            .expect("prepare source workflow");
        runner
            .activate_prepared_run(&source.prepared)
            .expect("activate source workflow");
        runner
            .state
            .record_agent_completed(
                &source.run_id,
                WorkflowAgentRecord {
                    call_id: "source-call".to_string(),
                    call_path: "main.agent".to_string(),
                    prompt: "source".to_string(),
                    opts: Value::Null,
                    team: None,
                    input_hash: "source-hash".to_string(),
                    status: WorkflowAgentStatus::Completed,
                    attempt: 1,
                    max_attempts: 1,
                    previous_errors: Vec::new(),
                    output: Some(Value::String("source result".to_string())),
                    error: None,
                    transcript_path: None,
                    started_at_ms: None,
                    completed_at_ms: None,
                    usage: Some(UsageTotals {
                        input_tokens: 30,
                        output_tokens: 10,
                        ..Default::default()
                    }),
                    task: None,
                    tool_events: Vec::new(),
                },
            )
            .expect("persist source usage");

        let resumed = runner
            .prepare_background(WorkflowLaunchRequest::from(WorkflowInput {
                script_path: Some(script_path.display().to_string()),
                resume_from_run_id: Some(source.run_id),
                ..Default::default()
            }))
            .expect("prepare resumed workflow");
        let launch = runner
            .activate_background(resumed)
            .expect("activate resumed workflow");

        assert_eq!(
            launch.output.budget,
            Some(WorkflowTokenBudget::from_total_and_spent(100, 40))
        );
        launch
            .join()
            .expect("resume worker thread")
            .expect("resume workflow result");
    }

    #[test]
    fn workflow_draft_resolution_preserves_token_budget() {
        let temp = tempdir().unwrap();
        let session_dir = temp.path().join("workflow-session");
        let draft_dir = session_dir.join("workflow-drafts").join("draft-budget");
        fs::create_dir_all(&draft_dir).unwrap();
        fs::write(
            draft_dir.join("script.js"),
            "export const meta = { name: 'draft-budget', description: 'budget', phases: ['main'] };\nexport default 'done';",
        )
        .unwrap();
        let mut config = test_run_config();
        config.cwd = Some(temp.path().to_path_buf());
        let runner = WorkflowRunner::new(
            config,
            TaskRegistry::new("workflow-draft-budget".to_string()),
            session_dir,
        );

        let resolved = runner
            .resolve_launch_input(&WorkflowInput {
                draft_id: Some("draft-budget".to_string()),
                token_budget: Some(321),
                ..Default::default()
            })
            .expect("draft input should resolve");

        assert_eq!(resolved.token_budget, Some(321));
    }

    #[test]
    fn workflow_launch_rejects_zero_token_budget() {
        let temp = tempdir().unwrap();
        let mut config = test_run_config();
        config.cwd = Some(temp.path().to_path_buf());
        let runner = WorkflowRunner::new(
            config,
            TaskRegistry::new("workflow-zero-budget".to_string()),
            temp.path().join("workflow-session"),
        );

        let error = runner
            .prepare_background(WorkflowLaunchRequest::from(WorkflowInput {
                script: Some(
                    "export const meta = { name: 'zero-budget', description: 'budget', phases: ['main'] };\nexport default 'done';".to_string(),
                ),
                token_budget: Some(0),
                ..Default::default()
            }))
            .expect_err("zero token budgets should be rejected");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("tokenBudget must be at least 1"));
    }

    #[test]
    fn durable_workflow_state_repairs_interrupted_task_for_every_terminal_outcome() {
        for (run_status, task_status) in [
            (WorkflowRunStatus::Completed, TaskStatus::Completed),
            (WorkflowRunStatus::Failed, TaskStatus::Failed),
            (WorkflowRunStatus::Stopped, TaskStatus::Stopped),
        ] {
            let temp = tempdir().unwrap();
            let workflow_dir = temp.path().join(".orca").join("workflows");
            fs::create_dir_all(&workflow_dir).unwrap();
            let script_path = workflow_dir.join("durable-terminal.js");
            fs::write(
                &script_path,
                "export const meta = { name: 'durable-terminal', description: 'durable terminal', phases: ['main'] };\nexport default 'done';",
            )
            .unwrap();
            let session_id = format!("workflow-terminal-{run_status:?}");
            let task_root = temp.path().join("task-sessions");
            let tasks =
                TaskRegistry::new_persistent(session_id.clone(), task_root.clone()).unwrap();
            let mut config = test_run_config();
            config.cwd = Some(temp.path().to_path_buf());
            let workflow_session = temp.path().join("workflow-session");
            let runner =
                WorkflowRunner::new(config.clone(), tasks.clone(), workflow_session.clone());
            let prepared = runner
                .prepare_background(WorkflowLaunchRequest::from(WorkflowInput {
                    script_path: Some(script_path.display().to_string()),
                    ..Default::default()
                }))
                .unwrap();
            runner
                .activate_prepared_run(&prepared.prepared)
                .expect("activate durable workflow fixture");
            let mut state = runner.state.load_run(&prepared.run_id).unwrap();
            state.status = run_status;
            match run_status {
                WorkflowRunStatus::Completed | WorkflowRunStatus::Stopped => {
                    state.final_summary = Some(format!("{run_status:?} result"));
                    state.error = None;
                }
                WorkflowRunStatus::Failed => {
                    state.final_summary = None;
                    state.error = Some("durable failure".to_string());
                }
                _ => unreachable!(),
            }
            runner.state.write_state(&state).unwrap();
            drop(runner);
            drop(tasks);

            let reopened_tasks =
                TaskRegistry::new_persistent(session_id, task_root.clone()).unwrap();
            assert_eq!(
                reopened_tasks.get(&prepared.task_id).unwrap().status,
                TaskStatus::Failed,
                "active TaskRegistry record must first expose the restart interruption window"
            );
            let reopened = WorkflowRunner::new(config, reopened_tasks, workflow_session.clone());
            assert!(
                reopened
                    .reconcile_durable_terminal_outcome(
                        &prepared.task_id,
                        "workflow-run-mismatched-owner",
                    )
                    .unwrap()
                    .is_none(),
                "a terminal task cannot authorize a different workflow run"
            );
            let repaired = reopened
                .reconcile_durable_terminal_outcome(&prepared.task_id, &prepared.run_id)
                .unwrap()
                .expect("durable workflow state must repair the interrupted task");
            assert_eq!(repaired.status, task_status);
            assert_eq!(
                reopened.state.load_run(&prepared.run_id).unwrap().status,
                run_status
            );
        }
    }

    #[test]
    fn missing_workflow_state_preserves_task_terminal_or_defers_restart_settlement() {
        for task_is_terminal in [false, true] {
            let temp = tempdir().unwrap();
            let workflow_dir = temp.path().join(".orca").join("workflows");
            fs::create_dir_all(&workflow_dir).unwrap();
            let script_path = workflow_dir.join("missing-state.js");
            fs::write(
                &script_path,
                "export const meta = { name: 'missing-state', description: 'missing state', phases: ['main'] };\nexport default 'done';",
            )
            .unwrap();
            let session_id = format!("workflow-missing-state-{task_is_terminal}");
            let task_root = temp.path().join("task-sessions");
            let tasks =
                TaskRegistry::new_persistent(session_id.clone(), task_root.clone()).unwrap();
            let mut config = test_run_config();
            config.cwd = Some(temp.path().to_path_buf());
            let workflow_session = temp.path().join("workflow-session");
            let runner =
                WorkflowRunner::new(config.clone(), tasks.clone(), workflow_session.clone());
            let prepared = runner
                .prepare_background(WorkflowLaunchRequest::from(WorkflowInput {
                    script_path: Some(script_path.display().to_string()),
                    ..Default::default()
                }))
                .unwrap();
            runner
                .activate_prepared_run(&prepared.prepared)
                .expect("persist task before simulating missing workflow state");
            fs::remove_file(runner.state.state_path(&prepared.run_id)).unwrap();
            if task_is_terminal {
                tasks
                    .complete(&prepared.task_id, "durable task result".to_string())
                    .unwrap();
            }
            drop(runner);
            drop(tasks);

            let reopened_tasks =
                TaskRegistry::new_persistent(session_id, task_root.clone()).unwrap();
            let reopened =
                WorkflowRunner::new(config, reopened_tasks.clone(), workflow_session.clone());
            let outcome = reopened
                .reconcile_durable_terminal_outcome(&prepared.task_id, &prepared.run_id)
                .expect("missing workflow state is a legal cross-store edge");
            if task_is_terminal {
                let outcome = outcome.expect("TaskRegistry terminal outcome remains authoritative");
                assert_eq!(outcome.status, TaskStatus::Completed);
                assert_eq!(outcome.result.as_deref(), Some("durable task result"));
            } else {
                assert!(
                    outcome.is_none(),
                    "interrupted active task must defer to generic restart settlement"
                );
                assert_eq!(
                    reopened_tasks.get(&prepared.task_id).unwrap().status,
                    TaskStatus::Failed
                );
            }
        }
    }

    #[test]
    fn workflow_child_agent_call_uses_injected_child_executor() {
        let temp = tempdir().unwrap();
        let mut config = test_run_config();
        config.cwd = Some(temp.path().to_path_buf());
        let runner = WorkflowRunner::new(
            config,
            TaskRegistry::new("workflow-injected-child".to_string()),
            temp.path().join("session"),
        )
        .with_child_executor(fake_workflow_child_executor);
        let workflow_ipc = WorkflowIpcContext::new();
        let policy = WorkflowAgentExecutionPolicy {
            max_agent_retries: 1,
            max_agent_tokens: None,
            allowed_tools: Some(vec!["shell".to_string()]),
            tool_policy_label: Some("test-policy".to_string()),
        };
        let call = AgentCall {
            call_id: "call-1".to_string(),
            call_path: "agent-a".to_string(),
            phase: Some("phase-a".to_string()),
            prompt: "inspect injected runner".to_string(),
            opts: serde_json::json!({}),
        };
        let cancel = CancelToken::new();

        let output = runner
            .run_child_agent_call(&call, &workflow_ipc, &policy, &cancel)
            .expect("injected child executor should satisfy workflow agent call");

        assert_eq!(output.message, "injected workflow child result");
    }

    #[test]
    fn workflow_execution_gate_normalizes_zero_concurrency() {
        let gate = Arc::new(WorkflowExecutionGate::new());
        let permit = gate
            .begin_agent(1, 0, None)
            .expect("zero concurrency should normalize to one worker");
        let counters = gate.snapshot().expect("gate counters");

        assert_eq!(counters.active_agents, 1);
        assert_eq!(counters.max_observed_concurrent_agents, 1);
        drop(permit);
    }

    #[test]
    fn workflow_execution_gate_rechecks_total_limit_after_waiting() {
        let gate = Arc::new(WorkflowExecutionGate::new());
        let first = gate
            .begin_agent(2, 1, None)
            .expect("first workflow agent should acquire the only active slot");
        let (result_tx, result_rx) = std::sync::mpsc::channel();

        thread::scope(|scope| {
            for _ in 0..2 {
                let gate = Arc::clone(&gate);
                let result_tx = result_tx.clone();
                scope.spawn(move || {
                    let result = gate.begin_agent(2, 1, None);
                    let acquired = result.is_ok();
                    result_tx.send(acquired).expect("send gate result");
                    thread::sleep(Duration::from_millis(25));
                    drop(result);
                });
            }

            thread::sleep(Duration::from_millis(25));
            drop(first);
        });
        drop(result_tx);

        let results = result_rx.into_iter().collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|acquired| **acquired).count(), 1);
        assert_eq!(results.iter().filter(|acquired| !**acquired).count(), 1);
        assert_eq!(gate.snapshot().expect("gate counters").total_agents, 2);
    }

    #[test]
    fn workflow_execution_gate_stops_new_agents_at_run_token_budget() {
        let gate = Arc::new(WorkflowExecutionGate::with_token_budget_and_spent(100, 0));
        let mut first = gate.begin_agent(10, 2, None).expect("first agent");
        first.settle_usage(80).expect("record first usage");
        drop(first);

        let mut second = gate
            .begin_agent(10, 2, None)
            .expect("80 percent warning threshold must not stop work");
        second.settle_usage(20).expect("record second usage");
        drop(second);

        let error = gate
            .begin_agent(10, 2, None)
            .expect_err("exhausted token budget must stop new agents");
        assert!(error.to_string().contains("100 token budget exhausted"));
        let budget = gate
            .budget()
            .expect("read budget snapshot")
            .expect("budget snapshot");
        assert_eq!(budget.total, 100);
        assert_eq!(budget.spent, 100);
        assert_eq!(budget.remaining, 0);
    }

    #[test]
    fn workflow_execution_gate_reserves_budget_before_concurrent_start() {
        let gate = Arc::new(WorkflowExecutionGate::with_token_budget_and_spent(100, 0));
        let mut first = gate
            .begin_agent(10, 2, Some(100))
            .expect("first agent should reserve the run budget");
        let (started_tx, started_rx) = std::sync::mpsc::channel();

        thread::scope(|scope| {
            let gate = Arc::clone(&gate);
            scope.spawn(move || {
                let result = gate.begin_agent(10, 2, Some(100));
                started_tx
                    .send(result.is_ok())
                    .expect("send reservation result");
                drop(result);
            });

            assert!(started_rx.recv_timeout(Duration::from_millis(25)).is_err());
            first
                .settle_usage(40)
                .expect("settle the first agent usage");
            drop(first);

            assert!(
                started_rx
                    .recv_timeout(Duration::from_secs(1))
                    .expect("second agent should start after reservation release")
            );
        });

        let counters = gate.snapshot().expect("read gate counters");
        assert_eq!(counters.settled_tokens, 40);
        assert_eq!(counters.reserved_tokens, 0);
    }

    #[test]
    fn workflow_status_line_reports_budget_warning_and_remaining_tokens() {
        let state = WorkflowRunState {
            run_id: "workflow-budget-status".to_string(),
            task_id: "task-budget-status".to_string(),
            session_id: "session-budget-status".to_string(),
            cwd: "/tmp".to_string(),
            workflow_name: "budget-status".to_string(),
            meta: orca_core::workflow_types::WorkflowMeta {
                name: "budget-status".to_string(),
                description: "budget status".to_string(),
                phases: vec!["main".to_string()],
                tags: Vec::new(),
                version: None,
            },
            script_digest: "digest".to_string(),
            args_digest: "args".to_string(),
            status: WorkflowRunStatus::Completed,
            phases: Vec::new(),
            total_agent_count: 1,
            final_summary: Some("done".to_string()),
            error: None,
        };
        let counts = WorkflowAgentStatusCounts::default();
        let counters = WorkflowExecutionCounters {
            max_observed_concurrent_agents: 1,
            ..Default::default()
        };
        let status = workflow_status_line(
            &state,
            &counts,
            &counters,
            2,
            Some(WorkflowTokenBudget::from_total_and_spent(100, 80)),
        );

        assert!(status.contains("Budget: 80 spent / 100 tokens (20 remaining)"));
        assert!(status.contains("warning"));

        let large_budget_status = workflow_status_line(
            &state,
            &counts,
            &counters,
            2,
            Some(WorkflowTokenBudget::from_total_and_spent(
                u64::MAX,
                u64::MAX / 10,
            )),
        );
        assert!(!large_budget_status.contains("warning"));
    }

    #[test]
    fn workflow_child_agent_observes_run_cancellation_token() {
        let temp = tempdir().unwrap();
        let mut config = test_run_config();
        config.cwd = Some(temp.path().to_path_buf());
        let runner = WorkflowRunner::new(
            config,
            TaskRegistry::new("workflow-cancelled-child".to_string()),
            temp.path().join("session"),
        )
        .with_child_executor(cancel_aware_workflow_child_executor);
        let workflow_ipc = WorkflowIpcContext::new();
        let policy = WorkflowAgentExecutionPolicy {
            max_agent_retries: 0,
            max_agent_tokens: None,
            allowed_tools: None,
            tool_policy_label: None,
        };
        let call = AgentCall {
            call_id: "call-cancel".to_string(),
            call_path: "agent-cancel".to_string(),
            phase: None,
            prompt: "wait for cancellation".to_string(),
            opts: serde_json::json!({}),
        };
        let cancel = CancelToken::new();
        let started = Instant::now();

        thread::scope(|scope| {
            let handle =
                scope.spawn(|| runner.run_child_agent_call(&call, &workflow_ipc, &policy, &cancel));
            thread::sleep(Duration::from_millis(100));
            cancel.cancel();
            let error = handle
                .join()
                .expect("child executor thread")
                .expect_err("cancelled child should not complete successfully");
            assert!(error.message.contains("cancelled"));
        });

        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn workflow_agent_delay_observes_pause_and_resume_requests() {
        let temp = tempdir().unwrap();
        let workflow_dir = temp.path().join(".orca").join("workflows");
        fs::create_dir_all(&workflow_dir).unwrap();
        let script_path = workflow_dir.join("pausable-delay.js");
        fs::write(
            &script_path,
            "export const meta = { name: 'pausable-delay', description: 'pause delay', phases: [] };\nexport default 'done';",
        )
        .unwrap();
        let mut config = test_run_config();
        config.cwd = Some(temp.path().to_path_buf());
        let runner = WorkflowRunner::new(
            config,
            TaskRegistry::new("workflow-pausable-delay".to_string()),
            temp.path().join("workflow-session"),
        );
        let prepared = runner
            .prepare_background(WorkflowLaunchRequest::from(WorkflowInput {
                script_path: Some(script_path.display().to_string()),
                ..Default::default()
            }))
            .unwrap();
        runner
            .activate_prepared_run(&prepared.prepared)
            .expect("activate pausable delay fixture");
        runner.state.request_pause(&prepared.run_id).unwrap();
        let cancel = CancelToken::new();

        thread::scope(|scope| {
            let delayed = scope.spawn(|| {
                runner.wait_for_agent_delay(
                    &prepared.run_id,
                    &prepared.task_id,
                    &cancel,
                    Duration::from_millis(500),
                )
            });
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                let state = runner.state.load_run(&prepared.run_id).unwrap();
                if state.status == WorkflowRunStatus::Paused {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "workflow agent delay never observed the pause request: {state:?}"
                );
                thread::sleep(Duration::from_millis(10));
            }
            runner.state.request_resume(&prepared.run_id).unwrap();
            assert!(
                !delayed.join().unwrap().unwrap(),
                "resuming a workflow delay must not cancel the workflow"
            );
        });
        assert_eq!(
            runner.state.load_run(&prepared.run_id).unwrap().status,
            WorkflowRunStatus::Running
        );
    }

    fn fake_workflow_child_executor(
        _config: &RunConfig,
        request: &ChildAgentRequest,
        _runtime: &mut ChildAgentRuntime<'_, SharedEventBuffer>,
        _cost_tracker: &mut CostTracker,
    ) -> io::Result<ChildAgentResult> {
        assert_eq!(request.prompt, "inspect injected runner");
        assert_eq!(
            request.allowed_tools.as_ref().map(|tools| tools.as_slice()),
            Some(vec!["shell".to_string()].as_slice())
        );
        assert_eq!(request.tool_policy_label.as_deref(), Some("test-policy"));
        assert!(request.workflow_ipc.is_some());
        Ok(ChildAgentResult {
            status: RunStatus::Success,
            final_message: Some("injected workflow child result".to_string()),
            error: None,
        })
    }

    fn cancel_aware_workflow_child_executor(
        _config: &RunConfig,
        _request: &ChildAgentRequest,
        runtime: &mut ChildAgentRuntime<'_, SharedEventBuffer>,
        _cost_tracker: &mut CostTracker,
    ) -> io::Result<ChildAgentResult> {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !runtime.cancel.is_cancelled() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        if !runtime.cancel.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "workflow child did not observe cancellation",
            ));
        }
        Ok(ChildAgentResult {
            status: RunStatus::Cancelled,
            final_message: None,
            error: Some("workflow child cancelled".to_string()),
        })
    }

    fn test_run_config() -> RunConfig {
        // Every test resolves ORCA_HOME to the process-wide isolated home so
        // parallel tests never contend with live `orca` processes or each
        // other's deleted temp dirs; an explicitly provided home (recovery
        // child fixture) is preserved.
        let _ = crate::history::claim_isolated_test_orca_home_if_unset();
        RunConfig {
            app_version: "0.0.0-test".to_string(),
            prompt: String::new(),
            cwd: None,
            output_format: OutputFormat::Jsonl,
            approval_mode: ApprovalMode::FullAuto,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::from_unchecked(Some("auto".to_string())),
            model_runtime: Default::default(),
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
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
            subagents: Default::default(),
            tools: ToolConfig::default(),
            workflows: WorkflowConfig::default(),
            theme: Default::default(),
            vim_mode: false,
            vim_insert_escape: None,
            update_check: false,
            desktop_notifications: false,
            terminal_notifications: false,
            auto_memory: false,
        }
    }
}

fn digest_value(value: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(
        serde_json::to_string(value)
            .unwrap_or_else(|_| "null".to_string())
            .as_bytes(),
    );
    format!("{:x}", hasher.finalize())
}

fn task_type_name(task_type: TaskType) -> &'static str {
    match task_type {
        TaskType::MainSession => "main_session",
        TaskType::Workflow => "workflow",
        TaskType::Subagent => "subagent",
        TaskType::Shell => "shell",
        TaskType::Monitor => "monitor",
    }
}
