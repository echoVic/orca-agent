use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use orca_core::cost_types::UsageTotals;
use orca_core::task_types::{TaskContinuationSummary, WorkflowAgentTaskSummary};
use orca_core::workflow_types::{
    WorkflowAgentFailureKind, WorkflowAgentStatus, WorkflowEvidenceAgent, WorkflowEvidenceBundle,
    WorkflowEvidenceFailure, WorkflowEvidenceFailureKind, WorkflowEvidenceIdentity,
    WorkflowEvidencePhase, WorkflowEvidenceToolEvent, WorkflowInput, WorkflowRunState,
    WorkflowRunStatus, WorkflowTaskLifecycleEvidence,
};
use orca_platform::fs::{AtomicWritePolicy, atomic_write};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkflowAgentCacheRecord {
    pub call_path: String,
    pub input_hash: String,
    pub output: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowAgentRecord {
    pub call_id: String,
    pub call_path: String,
    pub prompt: String,
    pub opts: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    pub input_hash: String,
    pub status: WorkflowAgentStatus,
    #[serde(default = "default_agent_attempt")]
    pub attempt: u32,
    #[serde(default = "default_agent_attempt")]
    pub max_attempts: u32,
    #[serde(default)]
    pub previous_errors: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_optional_agent_output")]
    pub output: Option<Value>,
    pub error: Option<String>,
    pub transcript_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<UsageTotals>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<WorkflowTaskLifecycleEvidence>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_events: Vec<WorkflowEvidenceToolEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<TaskContinuationSummary>,
}

#[derive(Clone, Debug)]
struct CachedWorkflowAgentRecord {
    record: WorkflowAgentRecord,
    output_present: bool,
}

impl CachedWorkflowAgentRecord {
    fn from_record(record: WorkflowAgentRecord) -> Self {
        let output_present = record.output.is_some();
        Self {
            record,
            output_present,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowAgentRecordOnDisk {
    call_id: String,
    call_path: String,
    prompt: String,
    opts: Value,
    #[serde(default)]
    team: Option<String>,
    input_hash: String,
    status: WorkflowAgentStatus,
    #[serde(default = "default_agent_attempt")]
    attempt: u32,
    #[serde(default = "default_agent_attempt")]
    max_attempts: u32,
    #[serde(default)]
    previous_errors: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_agent_output_field")]
    output: AgentOutputField,
    error: Option<String>,
    transcript_path: Option<String>,
    #[serde(default)]
    started_at_ms: Option<i64>,
    #[serde(default)]
    completed_at_ms: Option<i64>,
    #[serde(default)]
    usage: Option<UsageTotals>,
    #[serde(default)]
    task: Option<WorkflowTaskLifecycleEvidence>,
    #[serde(default)]
    tool_events: Vec<WorkflowEvidenceToolEvent>,
    #[serde(default)]
    continuation: Option<TaskContinuationSummary>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowAgentRecordOnDiskWritable {
    call_id: String,
    call_path: String,
    prompt: String,
    opts: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    team: Option<String>,
    input_hash: String,
    status: WorkflowAgentStatus,
    attempt: u32,
    max_attempts: u32,
    previous_errors: Vec<String>,
    #[serde(flatten)]
    output: AgentOutputField,
    error: Option<String>,
    transcript_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    started_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    completed_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usage: Option<UsageTotals>,
    #[serde(skip_serializing_if = "Option::is_none")]
    task: Option<WorkflowTaskLifecycleEvidence>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_events: Vec<WorkflowEvidenceToolEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    continuation: Option<TaskContinuationSummary>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
enum WorkflowAgentCacheFileOnDisk {
    Current(HashMap<String, WorkflowAgentRecordOnDisk>),
    Legacy(HashMap<String, WorkflowAgentCacheRecord>),
}

#[derive(Clone, Debug, Default)]
struct AgentOutputField {
    present: bool,
    value: Option<Value>,
}

impl Serialize for AgentOutputField {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(if self.present { Some(1) } else { Some(0) })?;
        if self.present {
            map.serialize_entry("output", &self.value)?;
        }
        map.end()
    }
}

#[derive(Clone, Debug)]
pub struct WorkflowStateStore {
    root: PathBuf,
    agent_cache_write_lock: Arc<Mutex<()>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowWorkerRecord {
    pub pid: u32,
    pub active: bool,
    pub started_at_ms: i64,
    pub completed_at_ms: Option<i64>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WorkflowAgentStatusCounts {
    pub completed: u32,
    pub failed: u32,
    pub cancelled: u32,
    pub cached: u32,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkflowControlRequest {
    #[serde(default)]
    stop_requested: bool,
    #[serde(default)]
    pause_requested: bool,
    #[serde(default)]
    updated_at_ms: i64,
}

impl WorkflowStateStore {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            agent_cache_write_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn run_dir(&self, run_id: &str) -> PathBuf {
        self.root.join(run_id)
    }

    pub fn transcript_dir(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("transcripts")
    }

    pub fn state_path(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("state.json")
    }

    pub fn launch_input_path(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("launch-input.json")
    }

    pub fn worker_path(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("worker.json")
    }

    pub fn stop_request_path(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("control.json")
    }

    pub fn mailbox_path(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("mailbox.json")
    }

    pub fn task_lists_path(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("task-lists.json")
    }

    pub fn evidence_path(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("evidence.json")
    }

    pub fn create_run(&self, state: &WorkflowRunState) -> io::Result<()> {
        fs::create_dir_all(self.transcript_dir(&state.run_id))?;
        self.write_state(state)
    }

    pub fn load_run(&self, run_id: &str) -> io::Result<WorkflowRunState> {
        self.load_state(run_id)
    }

    pub fn write_state(&self, state: &WorkflowRunState) -> io::Result<()> {
        let run_dir = self.run_dir(&state.run_id);
        fs::create_dir_all(&run_dir)?;
        write_json_pretty(&self.state_path(&state.run_id), state)
    }

    pub fn load_state(&self, run_id: &str) -> io::Result<WorkflowRunState> {
        read_workflow_run_state(&self.state_path(run_id))
    }

    pub fn write_evidence_bundle(&self, bundle: &WorkflowEvidenceBundle) -> io::Result<()> {
        write_json_pretty(&self.evidence_path(&bundle.run_id), bundle)
    }

    pub fn load_evidence_bundle(&self, run_id: &str) -> io::Result<WorkflowEvidenceBundle> {
        read_json(&self.evidence_path(run_id))
    }

    pub fn build_evidence_bundle(
        &self,
        state: &WorkflowRunState,
        identity: WorkflowEvidenceIdentity,
    ) -> io::Result<WorkflowEvidenceBundle> {
        let cache_path = self.run_dir(&state.run_id).join("agent-cache.json");
        let mut agents = if cache_path.exists() {
            read_agent_cache(&cache_path)?
                .into_values()
                .map(|entry| {
                    let failure_kind = agent_failure_kind(&entry.record);
                    let retry_attempted =
                        !entry.record.previous_errors.is_empty() || entry.record.attempt > 1;
                    let retryable = failure_kind.map(|kind| agent_failure_retryable(kind));
                    WorkflowEvidenceAgent {
                        call_id: entry.record.call_id,
                        call_path: entry.record.call_path,
                        team: entry
                            .record
                            .team
                            .or_else(|| workflow_agent_team(&entry.record.opts)),
                        barrier: workflow_agent_barrier(&entry.record.opts),
                        min_hold_ms: workflow_agent_min_hold_ms(&entry.record.opts),
                        input_hash: entry.record.input_hash,
                        status: entry.record.status,
                        attempt: entry.record.attempt,
                        max_attempts: entry.record.max_attempts,
                        previous_errors: entry.record.previous_errors,
                        error: entry.record.error,
                        transcript_path: entry.record.transcript_path,
                        started_at_ms: entry.record.started_at_ms,
                        completed_at_ms: entry.record.completed_at_ms,
                        usage: entry.record.usage,
                        task: entry.record.task,
                        tool_events: entry.record.tool_events,
                        failure_kind,
                        retryable,
                        retry_attempted,
                    }
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        agents.sort_by(|left, right| {
            left.call_path
                .cmp(&right.call_path)
                .then_with(|| left.call_id.cmp(&right.call_id))
        });
        let phases = state
            .phases
            .iter()
            .map(|phase| WorkflowEvidencePhase {
                name: phase.name.clone(),
                status: phase.status,
                started_at_ms: phase.started_at_ms,
                completed_at_ms: phase.completed_at_ms,
                agent_count: phase.agent_count,
                error: phase.error.clone(),
                fallback: phase.fallback.clone(),
            })
            .collect::<Vec<_>>();
        let failures = workflow_evidence_failures(state, &phases, &agents);

        Ok(WorkflowEvidenceBundle {
            evidence_version: 1,
            identity,
            run_id: state.run_id.clone(),
            task_id: state.task_id.clone(),
            session_id: state.session_id.clone(),
            cwd: state.cwd.clone(),
            workflow_name: state.workflow_name.clone(),
            meta: state.meta.clone(),
            script_digest: state.script_digest.clone(),
            args_digest: state.args_digest.clone(),
            status: state.status,
            phases,
            total_agent_count: state.total_agent_count,
            max_configured_concurrent_agents: 0,
            max_observed_concurrent_agents: 0,
            contract: None,
            final_summary: state.final_summary.clone(),
            error: state.error.clone(),
            agents,
            failures,
        })
    }

    pub fn write_launch_input(&self, run_id: &str, input: &WorkflowInput) -> io::Result<()> {
        write_json_pretty(&self.launch_input_path(run_id), input)
    }

    pub fn load_launch_input(&self, run_id: &str) -> io::Result<WorkflowInput> {
        read_json(&self.launch_input_path(run_id))
    }

    pub fn write_worker_record(
        &self,
        run_id: &str,
        worker: &WorkflowWorkerRecord,
    ) -> io::Result<()> {
        write_json_pretty(&self.worker_path(run_id), worker)
    }

    pub fn load_worker_record(&self, run_id: &str) -> io::Result<WorkflowWorkerRecord> {
        read_json(&self.worker_path(run_id))
    }

    pub fn mark_worker_exited(&self, run_id: &str) -> io::Result<()> {
        let mut worker = self.load_worker_record(run_id)?;
        worker.active = false;
        worker.completed_at_ms = Some(now_ms());
        self.write_worker_record(run_id, &worker)
    }

    pub fn request_stop(&self, run_id: &str) -> io::Result<()> {
        let mut control = self.load_control_request(run_id)?;
        control.stop_requested = true;
        control.updated_at_ms = now_ms();
        write_json_pretty(&self.stop_request_path(run_id), &control)
    }

    pub fn stop_requested(&self, run_id: &str) -> io::Result<bool> {
        Ok(self.load_control_request(run_id)?.stop_requested)
    }

    pub fn request_pause(&self, run_id: &str) -> io::Result<()> {
        let mut control = self.load_control_request(run_id)?;
        control.pause_requested = true;
        control.updated_at_ms = now_ms();
        write_json_pretty(&self.stop_request_path(run_id), &control)
    }

    pub fn request_resume(&self, run_id: &str) -> io::Result<()> {
        let mut control = self.load_control_request(run_id)?;
        control.pause_requested = false;
        control.updated_at_ms = now_ms();
        write_json_pretty(&self.stop_request_path(run_id), &control)?;
        let mut state = self.load_run(run_id)?;
        if state.status == WorkflowRunStatus::Paused {
            state.status = WorkflowRunStatus::Running;
            self.write_state(&state)?;
        }
        Ok(())
    }

    pub fn pause_requested(&self, run_id: &str) -> io::Result<bool> {
        Ok(self.load_control_request(run_id)?.pause_requested)
    }

    fn load_control_request(&self, run_id: &str) -> io::Result<WorkflowControlRequest> {
        let path = self.stop_request_path(run_id);
        if !path.exists() {
            return Ok(WorkflowControlRequest::default());
        }
        read_json(&path)
    }

    pub fn record_agent_completed(
        &self,
        run_id: &str,
        record: impl IntoWorkflowAgentRecord,
    ) -> io::Result<()> {
        let _lock = self
            .agent_cache_write_lock
            .lock()
            .map_err(|_| io::Error::other("workflow agent cache lock poisoned"))?;
        let record = record.into_workflow_agent_record();
        let path = self.run_dir(run_id).join("agent-cache.json");
        let mut cache = if path.exists() {
            read_agent_cache(&path)?
        } else {
            HashMap::new()
        };
        cache.insert(
            cache_key(&record.call_path, &record.input_hash),
            CachedWorkflowAgentRecord::from_record(record),
        );
        write_agent_cache(&path, &cache)
    }

    pub fn find_cached_agent(
        &self,
        run_id: &str,
        call_path: &str,
        input_hash: &str,
    ) -> Option<String> {
        self.find_cached_agent_value(run_id, call_path, input_hash)
            .map(|value| value_to_output_string(value))
    }

    pub fn find_cached_agent_value(
        &self,
        run_id: &str,
        call_path: &str,
        input_hash: &str,
    ) -> Option<Value> {
        let path = self.run_dir(run_id).join("agent-cache.json");
        if !path.exists() {
            return None;
        }

        let cache = read_agent_cache(&path).ok()?;
        cache
            .get(&cache_key(call_path, input_hash))
            .filter(|entry| is_reusable_cached_status(entry.record.status))
            .filter(|entry| entry.output_present)
            .map(|entry| entry.record.output.clone().unwrap_or(Value::Null))
    }

    pub fn find_agent_record(
        &self,
        run_id: &str,
        call_path: &str,
        input_hash: &str,
    ) -> Option<WorkflowAgentRecord> {
        let path = self.run_dir(run_id).join("agent-cache.json");
        if !path.exists() {
            return None;
        }
        read_agent_cache(&path)
            .ok()?
            .get(&cache_key(call_path, input_hash))
            .map(|entry| entry.record.clone())
    }

    pub fn cached_agent_result(
        &self,
        run_id: &str,
        call_path: &str,
        input_hash: &str,
    ) -> io::Result<Option<WorkflowAgentCacheRecord>> {
        let path = self.run_dir(run_id).join("agent-cache.json");
        if !path.exists() {
            return Ok(None);
        }

        let cache = read_agent_cache(&path)?;
        Ok(cache
            .get(&cache_key(call_path, input_hash))
            .filter(|entry| is_reusable_cached_status(entry.record.status))
            .filter(|entry| entry.output_present)
            .map(|entry| WorkflowAgentCacheRecord {
                call_path: entry.record.call_path.clone(),
                input_hash: entry.record.input_hash.clone(),
                output: entry.record.output.clone().unwrap_or(Value::Null),
            }))
    }

    pub fn agent_status_counts(&self, run_id: &str) -> io::Result<WorkflowAgentStatusCounts> {
        let path = self.run_dir(run_id).join("agent-cache.json");
        if !path.exists() {
            return Ok(WorkflowAgentStatusCounts::default());
        }

        let cache = read_agent_cache(&path)?;
        let mut counts = WorkflowAgentStatusCounts::default();
        for entry in cache.values() {
            match entry.record.status {
                WorkflowAgentStatus::Completed => {
                    counts.completed += 1;
                    counts.failed += entry.record.previous_errors.len() as u32;
                }
                WorkflowAgentStatus::Failed => {
                    counts.failed += entry.record.previous_errors.len() as u32 + 1;
                }
                WorkflowAgentStatus::Cancelled => counts.cancelled += 1,
                WorkflowAgentStatus::Cached => counts.cached += 1,
                WorkflowAgentStatus::Pending | WorkflowAgentStatus::Running => {}
            }
        }
        Ok(counts)
    }

    pub fn agent_summaries(&self, run_id: &str) -> io::Result<Vec<WorkflowAgentTaskSummary>> {
        let path = self.run_dir(run_id).join("agent-cache.json");
        if !path.exists() {
            return Ok(Vec::new());
        }

        let cache = read_agent_cache(&path)?;
        let mut summaries = cache
            .values()
            .map(|entry| WorkflowAgentTaskSummary {
                call_id: entry.record.call_id.clone(),
                call_path: entry.record.call_path.clone(),
                team: entry
                    .record
                    .team
                    .clone()
                    .or_else(|| workflow_agent_team(&entry.record.opts)),
                status: entry.record.status,
                attempt: entry.record.attempt,
                max_attempts: entry.record.max_attempts,
                previous_errors: entry.record.previous_errors.clone(),
                error: entry.record.error.clone(),
                transcript_path: entry.record.transcript_path.clone(),
                started_at_ms: entry.record.started_at_ms,
                completed_at_ms: entry.record.completed_at_ms,
                usage: entry.record.usage,
                continuation: entry.record.continuation.clone(),
            })
            .collect::<Vec<_>>();
        summaries.sort_by(|left, right| {
            left.call_path
                .cmp(&right.call_path)
                .then_with(|| left.call_id.cmp(&right.call_id))
        });
        Ok(summaries)
    }
}

fn is_reusable_cached_status(status: WorkflowAgentStatus) -> bool {
    matches!(
        status,
        WorkflowAgentStatus::Completed | WorkflowAgentStatus::Cached
    )
}

fn workflow_evidence_failures(
    state: &WorkflowRunState,
    phases: &[WorkflowEvidencePhase],
    agents: &[WorkflowEvidenceAgent],
) -> Vec<WorkflowEvidenceFailure> {
    let mut failures = Vec::new();
    for agent in agents {
        if let Some(failure_kind) = agent.failure_kind {
            failures.push(WorkflowEvidenceFailure {
                kind: WorkflowEvidenceFailureKind::AgentFailed,
                scope: agent_failure_scope(failure_kind).to_string(),
                phase_name: phase_name_from_call_path(&agent.call_path),
                call_id: Some(agent.call_id.clone()),
                call_path: Some(agent.call_path.clone()),
                message: agent
                    .error
                    .clone()
                    .or_else(|| agent.previous_errors.last().cloned()),
                retryable: agent.retryable,
                retry_attempted: agent.retry_attempted,
            });
        }
    }
    for phase in phases {
        if phase.status == WorkflowRunStatus::Failed {
            failures.push(WorkflowEvidenceFailure {
                kind: if phase.fallback.is_some() {
                    WorkflowEvidenceFailureKind::PhaseFailedContinue
                } else {
                    WorkflowEvidenceFailureKind::PhaseFailedBlocked
                },
                scope: "phase".to_string(),
                phase_name: Some(phase.name.clone()),
                call_id: None,
                call_path: None,
                message: phase.error.clone(),
                retryable: None,
                retry_attempted: false,
            });
        }
    }
    if state.status == WorkflowRunStatus::Failed {
        failures.push(WorkflowEvidenceFailure {
            kind: WorkflowEvidenceFailureKind::WorkflowFailed,
            scope: "workflow".to_string(),
            phase_name: None,
            call_id: None,
            call_path: None,
            message: state.error.clone(),
            retryable: None,
            retry_attempted: false,
        });
    }
    failures
}

fn agent_failure_kind(record: &WorkflowAgentRecord) -> Option<WorkflowAgentFailureKind> {
    if record.status != WorkflowAgentStatus::Failed && record.previous_errors.is_empty() {
        return None;
    }
    let message = record
        .error
        .as_deref()
        .or_else(|| record.previous_errors.last().map(String::as_str))
        .unwrap_or_default()
        .to_ascii_lowercase();
    if message.contains("mcp__") || message.contains("mcp") {
        Some(WorkflowAgentFailureKind::McpFailure)
    } else if message.contains("disallows tool")
        || message.contains("tool")
        || message.contains("command")
    {
        Some(WorkflowAgentFailureKind::ToolFailure)
    } else if message.contains("token budget") || message.contains("tokens exceeded") {
        Some(WorkflowAgentFailureKind::TokenBudget)
    } else if message.contains("schema validation") {
        Some(WorkflowAgentFailureKind::SchemaValidation)
    } else {
        Some(WorkflowAgentFailureKind::AgentFailed)
    }
}

fn agent_failure_retryable(kind: WorkflowAgentFailureKind) -> bool {
    matches!(kind, WorkflowAgentFailureKind::AgentFailed)
}

fn agent_failure_scope(kind: WorkflowAgentFailureKind) -> &'static str {
    match kind {
        WorkflowAgentFailureKind::AgentFailed => "agent",
        WorkflowAgentFailureKind::ToolFailure => "tool",
        WorkflowAgentFailureKind::McpFailure => "mcp",
        WorkflowAgentFailureKind::TokenBudget => "token_budget",
        WorkflowAgentFailureKind::SchemaValidation => "schema_validation",
    }
}

fn phase_name_from_call_path(call_path: &str) -> Option<String> {
    call_path
        .strip_prefix("phases.")
        .and_then(|rest| rest.split(':').next())
        .map(str::to_string)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn default_agent_attempt() -> u32 {
    1
}

pub trait IntoWorkflowAgentRecord {
    fn into_workflow_agent_record(self) -> WorkflowAgentRecord;
}

impl IntoWorkflowAgentRecord for WorkflowAgentRecord {
    fn into_workflow_agent_record(self) -> WorkflowAgentRecord {
        self
    }
}

impl IntoWorkflowAgentRecord for WorkflowAgentCacheRecord {
    fn into_workflow_agent_record(self) -> WorkflowAgentRecord {
        WorkflowAgentRecord {
            call_id: self.call_path.clone(),
            call_path: self.call_path,
            prompt: String::new(),
            opts: Value::Null,
            team: None,
            input_hash: self.input_hash,
            status: WorkflowAgentStatus::Completed,
            attempt: 1,
            max_attempts: 1,
            previous_errors: Vec::new(),
            output: Some(self.output),
            error: None,
            transcript_path: None,
            started_at_ms: None,
            completed_at_ms: None,
            usage: None,
            task: None,
            tool_events: Vec::new(),
            continuation: None,
        }
    }
}

impl From<&WorkflowAgentRecord> for WorkflowAgentCacheRecord {
    fn from(record: &WorkflowAgentRecord) -> Self {
        Self {
            call_path: record.call_path.clone(),
            input_hash: record.input_hash.clone(),
            output: record.output.clone().unwrap_or(Value::Null),
        }
    }
}

pub fn input_hash(prompt: &str, opts: &Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prompt.as_bytes());
    hasher.update(b"\0");
    hasher.update(
        serde_json::to_string(opts)
            .unwrap_or_else(|_| "null".to_string())
            .as_bytes(),
    );
    format!("{:x}", hasher.finalize())
}

fn cache_key(call_path: &str, input_hash: &str) -> String {
    format!("{call_path}:{input_hash}")
}

fn value_to_output_string(value: Value) -> String {
    match value {
        Value::String(output) => output,
        other => other.to_string(),
    }
}

fn deserialize_optional_agent_output<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum AgentOutputCompat {
        Value(Value),
        String(String),
    }

    Ok(
        match Option::<AgentOutputCompat>::deserialize(deserializer)? {
            Some(AgentOutputCompat::Value(value)) => Some(value),
            Some(AgentOutputCompat::String(output)) => Some(Value::String(output)),
            None => None,
        },
    )
}

fn deserialize_agent_output_field<'de, D>(deserializer: D) -> Result<AgentOutputField, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(AgentOutputField {
        present: true,
        value: deserialize_optional_agent_output(deserializer)?,
    })
}

fn read_agent_cache(path: &Path) -> io::Result<HashMap<String, CachedWorkflowAgentRecord>> {
    let content = retry_transient_workflow_read(
        || fs::read_to_string(path),
        std::time::Duration::from_millis(500),
    )?;
    let cache_file: WorkflowAgentCacheFileOnDisk = serde_json::from_str(&content)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    Ok(match cache_file {
        WorkflowAgentCacheFileOnDisk::Current(cache) => cache
            .into_iter()
            .map(|(key, record)| {
                (
                    key,
                    CachedWorkflowAgentRecord {
                        record: WorkflowAgentRecord {
                            call_id: record.call_id,
                            call_path: record.call_path,
                            prompt: record.prompt,
                            opts: record.opts,
                            team: record.team,
                            input_hash: record.input_hash,
                            status: record.status,
                            attempt: record.attempt,
                            max_attempts: record.max_attempts,
                            previous_errors: record.previous_errors,
                            output: record.output.value,
                            error: record.error,
                            transcript_path: record.transcript_path,
                            started_at_ms: record.started_at_ms,
                            completed_at_ms: record.completed_at_ms,
                            usage: record.usage,
                            task: record.task,
                            tool_events: record.tool_events,
                            continuation: record.continuation,
                        },
                        output_present: record.output.present,
                    },
                )
            })
            .collect(),
        WorkflowAgentCacheFileOnDisk::Legacy(cache) => cache
            .into_iter()
            .map(|(key, record)| {
                (
                    key,
                    CachedWorkflowAgentRecord {
                        record: record.into_workflow_agent_record(),
                        output_present: true,
                    },
                )
            })
            .collect(),
    })
}

fn write_agent_cache(
    path: &Path,
    cache: &HashMap<String, CachedWorkflowAgentRecord>,
) -> io::Result<()> {
    let on_disk: HashMap<String, WorkflowAgentRecordOnDiskWritable> = cache
        .iter()
        .map(|(key, entry)| {
            (
                key.clone(),
                WorkflowAgentRecordOnDiskWritable {
                    call_id: entry.record.call_id.clone(),
                    call_path: entry.record.call_path.clone(),
                    prompt: entry.record.prompt.clone(),
                    opts: entry.record.opts.clone(),
                    team: entry
                        .record
                        .team
                        .clone()
                        .or_else(|| workflow_agent_team(&entry.record.opts)),
                    input_hash: entry.record.input_hash.clone(),
                    status: entry.record.status,
                    attempt: entry.record.attempt,
                    max_attempts: entry.record.max_attempts,
                    previous_errors: entry.record.previous_errors.clone(),
                    output: AgentOutputField {
                        present: entry.output_present,
                        value: entry.record.output.clone(),
                    },
                    error: entry.record.error.clone(),
                    transcript_path: entry.record.transcript_path.clone(),
                    started_at_ms: entry.record.started_at_ms,
                    completed_at_ms: entry.record.completed_at_ms,
                    usage: entry.record.usage,
                    task: entry.record.task.clone(),
                    tool_events: entry.record.tool_events.clone(),
                    continuation: entry.record.continuation.clone(),
                },
            )
        })
        .collect();
    write_json_pretty(path, &on_disk)
}

pub fn workflow_agent_team(opts: &Value) -> Option<String> {
    opts.get("team")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|team| !team.is_empty())
        .map(str::to_string)
}

pub fn workflow_agent_barrier(opts: &Value) -> Option<String> {
    opts.get("barrier")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|barrier| !barrier.is_empty())
        .map(str::to_string)
}

pub fn workflow_agent_min_hold_ms(opts: &Value) -> Option<u64> {
    opts.get("minHoldMs")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0)
}

fn write_json_pretty<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    atomic_write(path, content.as_bytes(), AtomicWritePolicy::NoFollow).map_err(io::Error::other)
}

pub(crate) fn read_workflow_run_state(path: &Path) -> io::Result<WorkflowRunState> {
    read_json(path)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> io::Result<T> {
    let content = retry_transient_workflow_read(
        || fs::read_to_string(path),
        std::time::Duration::from_millis(500),
    )?;
    serde_json::from_str(&content)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn retry_transient_workflow_read<T>(
    mut operation: impl FnMut() -> io::Result<T>,
    timeout: std::time::Duration,
) -> io::Result<T> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match operation() {
            Err(error)
                if transient_workflow_read_error(&error)
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            result => return result,
        }
    }
}

fn transient_workflow_read_error(error: &io::Error) -> bool {
    #[cfg(windows)]
    {
        const ERROR_SHARING_VIOLATION: i32 = 32;
        const ERROR_LOCK_VIOLATION: i32 = 33;
        matches!(
            error.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
        ) || matches!(
            error.raw_os_error(),
            Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION)
        )
    }
    #[cfg(not(windows))]
    {
        let _ = error;
        false
    }
}

#[cfg(test)]
mod read_retry_tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn transient_workflow_read_retries_until_success() {
        let mut calls = 0;
        let value = retry_transient_workflow_read(
            || {
                calls += 1;
                if calls == 1 {
                    Err(io::Error::from(io::ErrorKind::NotFound))
                } else if calls == 2 {
                    Err(io::Error::from_raw_os_error(32))
                } else {
                    Ok("state")
                }
            },
            std::time::Duration::from_secs(1),
        )
        .unwrap();

        assert_eq!(value, "state");
        assert_eq!(calls, 3);
    }

    #[test]
    fn permanent_workflow_read_error_is_not_retried() {
        let mut calls = 0;
        let error = retry_transient_workflow_read(
            || {
                calls += 1;
                Err::<(), _>(io::Error::from(io::ErrorKind::InvalidData))
            },
            std::time::Duration::from_secs(1),
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(calls, 1);
    }
}
