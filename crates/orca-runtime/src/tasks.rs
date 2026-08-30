use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::process::Command;

use orca_core::cancel::CancelToken;
use orca_core::conversation::RawToolCall;
use orca_core::cost_types::UsageTotals;
use orca_core::provider_types::{
    ProviderError, ProviderErrorKind, ProviderResponse, ProviderStep, ToolCallProgress, Usage,
};
use orca_core::task_types::{
    BackgroundTaskSummary, PendingToolCallSummary, TaskActivitySummary, TaskContinuationSummary,
    TaskStatus, TaskType, WorkflowAgentTaskSummary, WorkflowPhaseTaskSummary, WorkflowTaskProgress,
};
use orca_core::thread_identity::TurnId;
use orca_core::thread_item_projection::ModelResponseIdentity;
use orca_core::tool_types::ToolRequest;
use orca_core::workflow_types::WorkflowInput;
use orca_platform::fs::{AtomicWritePolicy, ExclusiveFileLock, atomic_write};
use orca_platform::process::ProcessJob;
use serde::{Deserialize, Serialize};

use crate::agent_continuation::{
    AgentAttemptId, AgentCheckpointId, AgentContinuationId, AgentContinuationStore,
    ContinuationProjection, ContinuationRevision,
};
use crate::lifecycle::{
    RuntimeSubagentStatusLookup, RuntimeSubagentStatusRecord, RuntimeUsageTotals,
};
use crate::model_response::RuntimeModelResponse;
use crate::runtime_surface::{
    DisplayText, Sha256Digest, SurfaceTask, SurfaceTaskId, SurfaceTaskStatus, SurfaceTaskType,
    TaskRevision, UnixMillis, UsageTotals as SurfaceUsageTotals,
};
use crate::subagent_event_relay::{
    RelayError, RelayLease, RelayReadTarget, RelayRecord, SubagentEventRelay,
    SubagentEventRelayReader,
};
use crate::thread_store::redact_sensitive_text;

#[cfg(test)]
static TYPED_PROVIDER_OUTCOME_WRITE_FAILURES: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, usize>>,
> = std::sync::OnceLock::new();

const TASK_LEASE_DURATION_MS: i64 = 30_000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskLease {
    pub(crate) task_id: String,
    pub(crate) owner_id: String,
    pub(crate) epoch: u64,
    pub(crate) expires_at_ms: i64,
}

impl TaskLease {
    /// Returns the task identity guarded by this lease.
    #[allow(dead_code)] // Consumed by the detached-worker wiring in Task 4.
    pub(crate) fn task_id(&self) -> &str {
        &self.task_id
    }

    /// Returns the owner identity guarded by this lease.
    #[allow(dead_code)] // Consumed by the detached-worker wiring in Task 4.
    pub(crate) fn owner_id(&self) -> &str {
        &self.owner_id
    }

    /// Returns the monotonically increasing lease epoch.
    #[allow(dead_code)] // Consumed by the detached-worker wiring in Task 4.
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct WorkerExitFence {
    worker_pid: u32,
    adoption_revision: u64,
    lease_owner: Option<String>,
    lease_epoch: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskLeaseError {
    Held {
        owner_id: String,
        expires_at_ms: i64,
    },
    Fenced,
    NotFound,
    Terminal,
    Relay(RelayError),
    Persistence(String),
}

impl fmt::Display for TaskLeaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Held {
                owner_id,
                expires_at_ms,
            } => write!(
                formatter,
                "task lease is held by {owner_id} until {expires_at_ms}"
            ),
            Self::Fenced => formatter.write_str("task lease is no longer current"),
            Self::NotFound => formatter.write_str("task was not found"),
            Self::Terminal => formatter.write_str("task is already terminal"),
            Self::Relay(error) => write!(formatter, "subagent relay rejected event: {error}"),
            Self::Persistence(error) => write!(formatter, "task persistence failed: {error}"),
        }
    }
}

impl std::error::Error for TaskLeaseError {}

#[derive(Clone)]
pub struct TaskRegistry {
    session_id: String,
    owner_id: String,
    inner: Arc<Mutex<HashMap<String, TaskRecord>>>,
    cancelled_roots: Arc<Mutex<HashSet<String>>>,
    typed_provider_outcomes: Arc<Mutex<HashMap<String, DurableTypedProviderOutcome>>>,
    continuation_store: AgentContinuationStore,
    persistence: Option<Arc<TaskPersistence>>,
    persistent_open_error: Option<Arc<str>>,
    recover_persisted_active_tasks: bool,
    artifact_storage: Arc<TaskArtifactStorage>,
}

impl fmt::Debug for TaskRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TaskRegistry")
            .field("session_id", &self.session_id)
            .field("persistent", &self.persistence.is_some())
            .field("persistent_open_error", &self.persistent_open_error)
            .field(
                "recover_persisted_active_tasks",
                &self.recover_persisted_active_tasks,
            )
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
enum TaskArtifactStorage {
    Recorded,
    ProcessLocal {
        scratch: Mutex<Option<tempfile::TempDir>>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskHandle {
    pub id: String,
    pub task_type: TaskType,
    pub workflow_run_id: Option<String>,
}

#[derive(Clone, Debug)]
pub enum MainSessionTerminalUpdate {
    Completed {
        result: String,
    },
    Failed {
        error: String,
    },
    ApprovalRequired {
        summary: String,
        pending_tool_call: Option<PendingToolCallSummary>,
        pending_provider_response: Option<RuntimeModelResponse>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskTerminalTransition {
    pub is_backgrounded: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct LegacyTerminalTaskReconciliationRecord {
    id: String,
    status: TaskStatus,
    description: String,
    created_at_ms: i64,
    started_at_ms: Option<i64>,
    completed_at_ms: Option<i64>,
    usage: Option<UsageTotals>,
    result: Option<String>,
    error: Option<String>,
    retry_count: u32,
    output_truncated: bool,
    publication_revision: u64,
}

impl LegacyTerminalTaskReconciliationRecord {
    #[cfg(test)]
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    #[cfg(test)]
    pub(crate) fn status(&self) -> TaskStatus {
        self.status
    }

    #[cfg(test)]
    pub(crate) fn publication_revision(&self) -> u64 {
        self.publication_revision
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LegacyTerminalTaskReconciliationReceipt {
    session_id: String,
    publication_horizon: u64,
    digest: Sha256Digest,
    records: Vec<LegacyTerminalTaskReconciliationRecord>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct LegacyActiveTaskAdoptionRecord {
    id: String,
    description: String,
    created_at_ms: i64,
    started_at_ms: Option<i64>,
    usage: Option<UsageTotals>,
    retry_count: u32,
    output_truncated: bool,
    publication_revision: u64,
}

impl LegacyActiveTaskAdoptionRecord {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn description(&self) -> &str {
        &self.description
    }

    pub(crate) fn created_at_ms(&self) -> i64 {
        self.created_at_ms
    }

    pub(crate) fn started_at_ms(&self) -> Option<i64> {
        self.started_at_ms
    }

    pub(crate) fn usage(&self) -> Option<&UsageTotals> {
        self.usage.as_ref()
    }

    pub(crate) fn retry_count(&self) -> u32 {
        self.retry_count
    }

    pub(crate) fn output_truncated(&self) -> bool {
        self.output_truncated
    }

    pub(crate) fn publication_revision(&self) -> u64 {
        self.publication_revision
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LegacyActiveTaskAdoptionReceipt {
    session_id: String,
    publication_horizon: u64,
    digest: Sha256Digest,
    records: Vec<LegacyActiveTaskAdoptionRecord>,
}

impl LegacyActiveTaskAdoptionReceipt {
    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(crate) fn publication_horizon(&self) -> u64 {
        self.publication_horizon
    }

    pub(crate) fn records(&self) -> &[LegacyActiveTaskAdoptionRecord] {
        &self.records
    }

    pub(crate) fn is_valid(&self) -> bool {
        legacy_active_task_adoption_digest(
            &self.session_id,
            self.publication_horizon,
            &self.records,
        )
        .is_ok_and(|digest| digest == self.digest)
    }

    #[cfg(test)]
    pub(crate) fn digest(&self) -> Sha256Digest {
        self.digest.clone()
    }
}

fn legacy_active_task_adoption_digest(
    session_id: &str,
    publication_horizon: u64,
    records: &[LegacyActiveTaskAdoptionRecord],
) -> Result<Sha256Digest, serde_json::Error> {
    serde_json::to_vec(&(session_id, publication_horizon, records)).map(Sha256Digest::digest)
}

impl LegacyTerminalTaskReconciliationReceipt {
    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(crate) fn publication_horizon(&self) -> u64 {
        self.publication_horizon
    }

    #[cfg(test)]
    pub(crate) fn digest(&self) -> Sha256Digest {
        self.digest
    }

    #[cfg(test)]
    pub(crate) fn records(&self) -> &[LegacyTerminalTaskReconciliationRecord] {
        &self.records
    }

    pub(crate) fn is_valid(&self) -> bool {
        legacy_terminal_reconciliation_digest(
            &self.session_id,
            self.publication_horizon,
            &self.records,
        )
        .is_ok_and(|digest| digest == self.digest)
    }

    pub(crate) fn reconciled_surface_tasks(&self) -> Vec<SurfaceTask> {
        self.records
            .iter()
            .map(|record| {
                let status = match record.status {
                    TaskStatus::Completed => SurfaceTaskStatus::Completed,
                    TaskStatus::Stopped => SurfaceTaskStatus::Stopped,
                    TaskStatus::Cancelled => SurfaceTaskStatus::Cancelled,
                    _ => unreachable!("receipt contains only non-retryable terminal tasks"),
                };
                SurfaceTask {
                    task_id: SurfaceTaskId::try_new(record.id.clone())
                        .expect("receipt validated the surface task id"),
                    revision: TaskRevision::try_new(1).expect("one is a valid task revision"),
                    task_type: SurfaceTaskType::MainSession,
                    status,
                    backgrounded: false,
                    description: DisplayText::new(&record.description),
                    created_at: UnixMillis::new(record.created_at_ms),
                    started_at: record.started_at_ms.map(UnixMillis::new),
                    completed_at: record.completed_at_ms.map(UnixMillis::new),
                    parent_operation: None,
                    parent_task_id: None,
                    background_fence: None,
                    workflow_run_id: None,
                    subagent_id: None,
                    pending_interaction_id: None,
                    usage: record.usage.map(|usage| SurfaceUsageTotals {
                        input_tokens: usage.input_tokens,
                        output_tokens: usage.output_tokens,
                        cache_tokens: usage.cache_tokens,
                        estimated_cost_usd_micros: crate::cost::usd_to_micros(
                            usage.estimated_cost_usd,
                        ),
                    }),
                    result: record.result.as_deref().map(DisplayText::new),
                    error: record.error.as_deref().map(DisplayText::new),
                    retry_count: record.retry_count,
                    output_truncated: record.output_truncated,
                }
            })
            .collect()
    }
}

fn legacy_terminal_reconciliation_digest(
    session_id: &str,
    publication_horizon: u64,
    records: &[LegacyTerminalTaskReconciliationRecord],
) -> Result<Sha256Digest, serde_json::Error> {
    serde_json::to_vec(&(session_id, publication_horizon, records)).map(Sha256Digest::digest)
}

#[derive(Clone, Debug)]
pub struct TaskRecord {
    pub id: String,
    pub parent_task_id: Option<String>,
    pub task_type: TaskType,
    pub status: TaskStatus,
    pub is_backgrounded: bool,
    pub description: String,
    pub created_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub completed_at_ms: Option<i64>,
    pub name: Option<String>,
    pub agent_type: Option<String>,
    pub tool: Option<String>,
    pub pending_tool_call: Option<PendingToolCallSummary>,
    pub pending_tool_approval_response: Option<bool>,
    pub pending_provider_response: Option<RuntimeModelResponse>,
    pub workflow_run_id: Option<String>,
    pub phase_count: Option<usize>,
    pub workflow_progress: Option<WorkflowTaskProgress>,
    pub workflow_phases: Vec<WorkflowPhaseTaskSummary>,
    pub workflow_agents: Vec<WorkflowAgentTaskSummary>,
    pub workflow_script_path: Option<String>,
    pub workflow_launch_input: Option<WorkflowInput>,
    pub workflow_final_summary: Option<String>,
    pub workflow_failure_count: u32,
    pub usage: Option<UsageTotals>,
    pub subagent_current_activity: Option<String>,
    pub subagent_turn: Option<u32>,
    pub last_activity_at_ms: Option<i64>,
    pub(crate) continuation_id: Option<AgentContinuationId>,
    pub(crate) continuation_attempt_id: Option<AgentAttemptId>,
    pub(crate) continuation_checkpoint_id: Option<AgentCheckpointId>,
    pub(crate) continuation_revision: Option<ContinuationRevision>,
    pub(crate) continuation_resumable: bool,
    pub(crate) continuation_indeterminate: bool,
    pub result: Option<String>,
    pub error: Option<String>,
    pub retry_count: u32,
    pub output_truncated: bool,
    pub worker_pid: Option<u32>,
    pub command: Option<String>,
    pub lease_owner: Option<String>,
    pub lease_epoch: u64,
    pub lease_expires_at_ms: Option<i64>,
    pub stop_requested: bool,
    pub publication_revision: u64,
    pub control: TaskControl,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TaskContinuationProjection {
    pub(crate) continuation_id: AgentContinuationId,
    pub(crate) attempt_id: AgentAttemptId,
    pub(crate) checkpoint_id: Option<AgentCheckpointId>,
    pub(crate) revision: ContinuationRevision,
    pub(crate) resumable: bool,
    pub(crate) indeterminate: bool,
}

impl From<&ContinuationProjection> for TaskContinuationProjection {
    fn from(projection: &ContinuationProjection) -> Self {
        Self {
            continuation_id: projection.continuation_id.clone(),
            attempt_id: projection.attempt_id.clone(),
            checkpoint_id: projection.checkpoint_id.clone(),
            revision: projection.revision,
            resumable: projection.resumable,
            indeterminate: projection.indeterminate,
        }
    }
}

#[derive(Clone, Debug)]
pub struct TaskControl {
    pub cancel: CancelToken,
    pub pause: Arc<AtomicBool>,
    worker: Arc<Mutex<Option<OwnedWorker>>>,
}

#[derive(Debug)]
struct OwnedWorker {
    child: Child,
    process_job: ProcessJob,
}

enum TaskStopTarget {
    InProcess,
    Owned {
        worker: Arc<Mutex<Option<OwnedWorker>>>,
        owned_worker: OwnedWorker,
    },
    Recovered {
        pid: u32,
        agent_id: String,
    },
}

enum RecoveredWorkerState {
    Missing,
    Matches,
    Replaced,
}

#[cfg(windows)]
pub(crate) fn async_worker_job_name(agent_id: &str) -> String {
    format!(r"Local\Orca.AsyncWorker.{agent_id}")
}
#[derive(Clone, Debug)]
struct TaskPersistence {
    root: PathBuf,
    session_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PersistedTaskRecord {
    id: String,
    #[serde(default)]
    parent_task_id: Option<String>,
    task_type: TaskType,
    status: TaskStatus,
    #[serde(default)]
    is_backgrounded: bool,
    description: String,
    created_at_ms: i64,
    started_at_ms: Option<i64>,
    completed_at_ms: Option<i64>,
    name: Option<String>,
    agent_type: Option<String>,
    #[serde(default)]
    tool: Option<String>,
    #[serde(default)]
    pending_tool_call: Option<PendingToolCallSummary>,
    #[serde(default)]
    pending_provider_response: Option<serde_json::Value>,
    #[serde(default)]
    pending_tool_approval_response: Option<bool>,
    workflow_run_id: Option<String>,
    phase_count: Option<usize>,
    workflow_progress: Option<WorkflowTaskProgress>,
    #[serde(default)]
    workflow_phases: Vec<WorkflowPhaseTaskSummary>,
    #[serde(default)]
    workflow_agents: Vec<WorkflowAgentTaskSummary>,
    #[serde(default)]
    workflow_script_path: Option<String>,
    #[serde(default)]
    workflow_launch_input: Option<WorkflowInput>,
    #[serde(default)]
    workflow_final_summary: Option<String>,
    #[serde(default)]
    workflow_failure_count: u32,
    usage: Option<UsageTotals>,
    #[serde(default)]
    subagent_current_activity: Option<String>,
    #[serde(default)]
    subagent_turn: Option<u32>,
    #[serde(default)]
    last_activity_at_ms: Option<i64>,
    #[serde(default)]
    continuation_id: Option<AgentContinuationId>,
    #[serde(default)]
    continuation_attempt_id: Option<AgentAttemptId>,
    #[serde(default)]
    continuation_checkpoint_id: Option<AgentCheckpointId>,
    #[serde(default)]
    continuation_revision: Option<ContinuationRevision>,
    #[serde(default)]
    continuation_resumable: bool,
    #[serde(default)]
    continuation_indeterminate: bool,
    result: Option<String>,
    error: Option<String>,
    #[serde(default)]
    retry_count: u32,
    #[serde(default)]
    output_truncated: bool,
    #[serde(default)]
    worker_pid: Option<u32>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    lease_owner: Option<String>,
    #[serde(default)]
    lease_epoch: u64,
    #[serde(default)]
    lease_expires_at_ms: Option<i64>,
    #[serde(default)]
    stop_requested: bool,
    #[serde(default)]
    publication_revision: u64,
}

#[derive(Clone, Debug)]
pub(crate) struct DurableTypedProviderOutcome {
    pub status: TaskStatus,
    pub response: Option<RuntimeModelResponse>,
    pub error: Option<String>,
    pub usage: Option<UsageTotals>,
    pub operation_terminal: Option<orca_core::budget::OperationTerminal>,
    pub completed_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PersistedTypedProviderOutcome {
    status: TaskStatus,
    #[serde(default)]
    response: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    usage: Option<UsageTotals>,
    #[serde(default)]
    operation_terminal: Option<orca_core::budget::OperationTerminal>,
    completed_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct PersistedProviderResponse {
    #[serde(default)]
    steps: Vec<PersistedProviderStep>,
    assistant_content: Option<String>,
    assistant_reasoning: Option<String>,
    #[serde(default)]
    tool_calls: Vec<RawToolCall>,
    usage: Option<Usage>,
    #[serde(default)]
    identity: Option<ModelResponseIdentity>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
enum PersistedProviderStep {
    ReasoningDelta(String),
    MessageDelta(String),
    ToolCallProgress(ToolCallProgress),
    ToolCall(ToolRequest),
    Error(PersistedProviderError),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum PersistedProviderError {
    Legacy(String),
    Classified {
        kind: ProviderErrorKind,
        message: String,
    },
}

impl TaskRegistry {
    /// Injects `count` typed-provider-outcome persistence failures for
    /// `session_id` only, so parallel tests never consume each other's
    /// injection budget.
    #[cfg(test)]
    pub(crate) fn inject_typed_provider_outcome_write_failures(session_id: &str, count: usize) {
        TYPED_PROVIDER_OUTCOME_WRITE_FAILURES
            .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(session_id.to_string(), count);
    }

    pub fn new(session_id: String) -> Self {
        let continuation_store = AgentContinuationStore::new(session_id.clone());
        Self {
            session_id,
            owner_id: new_task_lease_owner_id(std::process::id()),
            inner: Arc::new(Mutex::new(HashMap::new())),
            cancelled_roots: Arc::new(Mutex::new(HashSet::new())),
            typed_provider_outcomes: Arc::new(Mutex::new(HashMap::new())),
            continuation_store,
            persistence: None,
            persistent_open_error: None,
            recover_persisted_active_tasks: false,
            artifact_storage: Arc::new(TaskArtifactStorage::ProcessLocal {
                scratch: Mutex::new(None),
            }),
        }
    }

    pub fn new_persistent(session_id: String, root: PathBuf) -> io::Result<Self> {
        Self::open_persistent(session_id, root, true)
    }

    fn new_persistent_attached(session_id: String, root: PathBuf) -> io::Result<Self> {
        Self::open_persistent(session_id, root, false)
    }

    fn open_persistent(
        session_id: String,
        root: PathBuf,
        recover_interrupted: bool,
    ) -> io::Result<Self> {
        let continuation_store =
            AgentContinuationStore::new_persistent(session_id.clone(), root.clone())
                .map_err(io::Error::other)?;
        let persistence = Arc::new(TaskPersistence::new(root, session_id.clone()));
        let continuation_reconciliation = if recover_interrupted {
            Some(
                continuation_store
                    .reconcile_expired_owners_locked()
                    .map_err(io::Error::other)?,
            )
        } else {
            None
        };
        let _session_lock = if continuation_reconciliation.is_none() {
            Some(
                ExclusiveFileLock::acquire(&persistence.session_lock_path(&session_id))
                    .map_err(io::Error::other)?,
            )
        } else {
            None
        };
        let typed_provider_outcomes =
            persistence.load_typed_provider_outcomes_unlocked(&session_id)?;
        let mut records = persistence.load_session_records_unlocked(&session_id)?;
        let mut changed = false;
        if recover_interrupted {
            let continuation_projections = continuation_reconciliation
                .as_ref()
                .map(|reconciliation| reconciliation.projections())
                .unwrap_or_default();
            let reconciled_task_ids = continuation_projections
                .iter()
                .map(|projection| projection.latest_task_id.clone())
                .collect::<HashSet<_>>();
            for projection in continuation_projections {
                let Some(record) = records.get_mut(&projection.latest_task_id) else {
                    continue;
                };
                let task_projection = TaskContinuationProjection::from(projection);
                if record.continuation_projection().as_ref() != Some(&task_projection) {
                    record.set_continuation_projection(task_projection);
                    record.publication_revision = record.publication_revision.saturating_add(1);
                    changed = true;
                }
                if !is_terminal(record.status) && (projection.resumable || projection.indeterminate)
                {
                    record.status = TaskStatus::Failed;
                    record.completed_at_ms = Some(now_ms());
                    record.worker_pid = None;
                    record.lease_owner = None;
                    record.lease_expires_at_ms = None;
                    record.result = None;
                    record.error = Some(if projection.indeterminate {
                        "subagent continuation is indeterminate after owner expiry; inspect external state before retrying"
                            .to_string()
                    } else {
                        format!(
                            "subagent interrupted after a safe checkpoint; resume_from={}",
                            projection.continuation_id
                        )
                    });
                    record.publication_revision = record.publication_revision.saturating_add(1);
                    changed = true;
                }
            }
            for (task_id, record) in &mut records {
                if !reconciled_task_ids.contains(task_id)
                    && !typed_provider_outcomes.contains_key(task_id)
                {
                    changed |= mark_interrupted_if_active(record);
                }
            }
        }
        if changed {
            persistence.write_session_records_unlocked(&session_id, &records)?;
        }
        drop(_session_lock);
        Ok(Self {
            session_id,
            owner_id: new_task_lease_owner_id(std::process::id()),
            inner: Arc::new(Mutex::new(records)),
            cancelled_roots: Arc::new(Mutex::new(HashSet::new())),
            typed_provider_outcomes: Arc::new(Mutex::new(typed_provider_outcomes)),
            continuation_store,
            persistence: Some(persistence),
            persistent_open_error: None,
            recover_persisted_active_tasks: recover_interrupted,
            artifact_storage: Arc::new(TaskArtifactStorage::Recorded),
        })
    }

    pub(crate) fn attach_for_cwd(session_id: String, cwd: &Path) -> Self {
        let Some(root) = task_sessions_root() else {
            let mut registry = Self::new(session_id);
            registry.artifact_storage = Arc::new(TaskArtifactStorage::Recorded);
            return registry;
        };
        let legacy_root = legacy_project_task_sessions_root(cwd);
        let _ = migrate_legacy_task_sessions(&legacy_root, &root);
        Self::new_persistent_attached(session_id.clone(), root).unwrap_or_else(|error| {
            let mut registry = Self::new(session_id);
            registry.persistent_open_error = Some(Arc::from(error.to_string()));
            registry.artifact_storage = Arc::new(TaskArtifactStorage::Recorded);
            registry
        })
    }

    pub fn new_for_cwd(session_id: String, cwd: &Path) -> Self {
        let Some(root) = task_sessions_root() else {
            let mut registry = Self::new(session_id);
            registry.artifact_storage = Arc::new(TaskArtifactStorage::Recorded);
            return registry;
        };
        let legacy_root = legacy_project_task_sessions_root(cwd);
        let _ = migrate_legacy_task_sessions(&legacy_root, &root);
        Self::new_persistent(session_id.clone(), root).unwrap_or_else(|error| {
            let mut registry = Self::new(session_id);
            registry.persistent_open_error = Some(Arc::from(error.to_string()));
            registry.artifact_storage = Arc::new(TaskArtifactStorage::Recorded);
            registry
        })
    }

    pub(crate) fn is_process_local(&self) -> bool {
        matches!(
            self.artifact_storage.as_ref(),
            TaskArtifactStorage::ProcessLocal { .. }
        )
    }

    pub(crate) fn workflow_session_dir(&self, cwd: &Path) -> io::Result<PathBuf> {
        match self.artifact_storage.as_ref() {
            TaskArtifactStorage::Recorded => Ok(cwd
                .join(".orca")
                .join("workflow-sessions")
                .join(self.session_id())),
            TaskArtifactStorage::ProcessLocal { scratch } => {
                let mut scratch = scratch
                    .lock()
                    .map_err(|_| io::Error::other("runtime artifact storage lock poisoned"))?;
                if scratch.is_none() {
                    *scratch = Some(
                        tempfile::Builder::new()
                            .prefix("orca-ephemeral-runtime-")
                            .tempdir()?,
                    );
                }
                Ok(scratch
                    .as_ref()
                    .expect("process-local scratch was initialized")
                    .path()
                    .join("workflow-session"))
            }
        }
    }

    pub(crate) fn release_process_local_artifacts(&self) {
        if let TaskArtifactStorage::ProcessLocal { scratch } = self.artifact_storage.as_ref()
            && let Ok(mut scratch) = scratch.lock()
        {
            *scratch = None;
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub(crate) fn continuation_store(&self) -> Result<AgentContinuationStore, String> {
        if let Some(error) = self.persistent_open_error.as_deref() {
            return Err(format!("persistent task registry is unavailable: {error}"));
        }
        Ok(self.continuation_store.clone())
    }

    pub(crate) fn install_continuation_projection(
        &self,
        id: &str,
        projection: &ContinuationProjection,
    ) -> Result<(), String> {
        if let Some(error) = self.persistent_open_error.as_deref() {
            return Err(format!("persistent task registry is unavailable: {error}"));
        }
        if projection.latest_task_id != id {
            return Err(format!(
                "continuation projection targets task '{}', not '{id}'",
                projection.latest_task_id
            ));
        }
        let projection = TaskContinuationProjection::from(projection);
        self.mutate_task(id, move |record| {
            let changed = record.continuation_projection().as_ref() != Some(&projection);
            if changed {
                record.set_continuation_projection(projection);
            }
            Ok(((), changed))
        })
    }

    pub(crate) fn continuation_projection(
        &self,
        id: &str,
    ) -> Result<Option<TaskContinuationProjection>, String> {
        if let Some(error) = self.persistent_open_error.as_deref() {
            return Err(format!("persistent task registry is unavailable: {error}"));
        }
        let record = if self.persistence.is_some() {
            self.refresh_task_from_persistence(id)?
        } else {
            self.get(id)
        };
        Ok(record.and_then(|record| record.continuation_projection()))
    }

    pub(crate) fn with_terminal_main_session_reconciliation<R>(
        &self,
        reconcile: impl FnOnce(&LegacyTerminalTaskReconciliationReceipt) -> R,
    ) -> Result<Option<R>, String> {
        if let Some(error) = self.persistent_open_error.as_deref() {
            return Err(format!("persistent task registry is unavailable: {error}"));
        }
        let Some(persistence) = self.persistence.as_ref() else {
            return Ok(None);
        };
        let _session_lock =
            ExclusiveFileLock::acquire(&persistence.session_lock_path(&self.session_id))
                .map_err(|error| error.to_string())?;
        let persisted = persistence
            .load_session_records_unlocked(&self.session_id)
            .map_err(|error| error.to_string())?;
        let mut records = persisted
            .into_values()
            .filter(terminal_main_session_reconciliation_eligible)
            .map(|record| LegacyTerminalTaskReconciliationRecord {
                id: record.id,
                status: record.status,
                description: record.description,
                created_at_ms: record.created_at_ms,
                started_at_ms: record.started_at_ms,
                completed_at_ms: record.completed_at_ms,
                usage: record.usage,
                result: record.result,
                error: record.error,
                retry_count: record.retry_count,
                output_truncated: record.output_truncated,
                publication_revision: record.publication_revision,
            })
            .collect::<Vec<_>>();
        records.sort_by(|left, right| left.id.cmp(&right.id));
        let publication_horizon = records
            .iter()
            .map(|record| record.publication_revision)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| "legacy task publication revision exhausted".to_string())?;
        let digest =
            legacy_terminal_reconciliation_digest(&self.session_id, publication_horizon, &records)
                .map_err(|error| error.to_string())?;
        let receipt = LegacyTerminalTaskReconciliationReceipt {
            session_id: self.session_id.clone(),
            publication_horizon,
            digest,
            records,
        };
        Ok(Some(reconcile(&receipt)))
    }

    pub(crate) fn with_active_main_session_adoption<R>(
        &self,
        adopt: impl FnOnce(&LegacyActiveTaskAdoptionReceipt) -> R,
    ) -> Result<Option<R>, String> {
        if let Some(error) = self.persistent_open_error.as_deref() {
            return Err(format!("persistent task registry is unavailable: {error}"));
        }
        let Some(persistence) = self.persistence.as_ref() else {
            return Ok(None);
        };
        let _session_lock =
            ExclusiveFileLock::acquire(&persistence.session_lock_path(&self.session_id))
                .map_err(|error| error.to_string())?;
        let persisted = persistence
            .load_session_records_unlocked(&self.session_id)
            .map_err(|error| error.to_string())?;
        let typed_provider_outcomes = persistence
            .load_typed_provider_outcomes_unlocked(&self.session_id)
            .map_err(|error| error.to_string())?;
        let mut records = persisted
            .into_values()
            .filter(|record| {
                active_main_session_adoption_eligible(record)
                    && !typed_provider_outcomes.contains_key(&record.id)
            })
            .map(|record| LegacyActiveTaskAdoptionRecord {
                id: record.id,
                description: record.description,
                created_at_ms: record.created_at_ms,
                started_at_ms: record.started_at_ms,
                usage: record.usage,
                retry_count: record.retry_count,
                output_truncated: record.output_truncated,
                publication_revision: record.publication_revision,
            })
            .collect::<Vec<_>>();
        records.sort_by(|left, right| left.id.cmp(&right.id));
        let publication_horizon = records
            .iter()
            .map(|record| record.publication_revision)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| "legacy task publication revision exhausted".to_string())?;
        let digest =
            legacy_active_task_adoption_digest(&self.session_id, publication_horizon, &records)
                .map_err(|error| error.to_string())?;
        let receipt = LegacyActiveTaskAdoptionReceipt {
            session_id: self.session_id.clone(),
            publication_horizon,
            digest,
            records,
        };
        Ok(Some(adopt(&receipt)))
    }

    pub fn acquire_task_lease(&self, id: &str) -> Result<TaskLease, TaskLeaseError> {
        let owner_id = self.owner_id.clone();
        self.mutate_task_for_lease(id, move |record| {
            if is_terminal(record.status) {
                return Err(TaskLeaseError::Terminal);
            }
            let now = now_ms();
            if let (Some(current_owner), Some(expires_at_ms)) =
                (&record.lease_owner, record.lease_expires_at_ms)
                && current_owner != &owner_id
                && expires_at_ms > now
            {
                return Err(TaskLeaseError::Held {
                    owner_id: current_owner.clone(),
                    expires_at_ms,
                });
            }
            record.lease_epoch = record.lease_epoch.saturating_add(1).max(1);
            let expires_at_ms = now.saturating_add(TASK_LEASE_DURATION_MS);
            record.lease_owner = Some(owner_id.clone());
            record.lease_expires_at_ms = Some(expires_at_ms);
            record.publication_revision = record.publication_revision.saturating_add(1);
            Ok(TaskLease {
                task_id: record.id.clone(),
                owner_id: owner_id.clone(),
                epoch: record.lease_epoch,
                expires_at_ms,
            })
        })
    }

    pub fn mark_running_with_lease(
        &self,
        lease: &TaskLease,
        id: &str,
    ) -> Result<(), TaskLeaseError> {
        let owner_id = self.owner_id.clone();
        self.mutate_task_for_lease(id, |record| {
            validate_task_lease(record, lease, &owner_id)?;
            if record.stop_requested {
                return Err(TaskLeaseError::Fenced);
            }
            record.status = TaskStatus::Running;
            if record.started_at_ms.is_none() {
                record.started_at_ms = Some(now_ms());
            }
            record.completed_at_ms = None;
            record.control.pause.store(false, Ordering::Release);
            record.publication_revision = record.publication_revision.saturating_add(1);
            Ok(())
        })
    }

    pub fn renew_task_lease(&self, lease: &TaskLease, id: &str) -> Result<(), TaskLeaseError> {
        let owner_id = self.owner_id.clone();
        self.mutate_task_for_lease(id, |record| {
            validate_task_lease(record, lease, &owner_id)?;
            record.lease_expires_at_ms = Some(now_ms().saturating_add(TASK_LEASE_DURATION_MS));
            record.publication_revision = record.publication_revision.saturating_add(1);
            Ok(())
        })
    }

    /// Appends a detached subagent event while holding the task's current
    /// lease mutation boundary.  The relay itself is the durable delivery
    /// journal; this method only admits a record when the task owner, lease
    /// epoch, task type, and current continuation attempt all agree.
    #[allow(dead_code)] // Wired into the detached worker by Task 4.
    pub(crate) fn append_subagent_event_with_lease(
        &self,
        lease: &TaskLease,
        id: &str,
        attempt_id: &str,
        record: RelayRecord,
    ) -> Result<crate::subagent_event_relay::AppendResult, TaskLeaseError> {
        let owner_id = self.owner_id.clone();
        let root = self
            .persistence
            .as_ref()
            .map(|persistence| persistence.root.clone())
            .ok_or_else(|| {
                TaskLeaseError::Persistence(
                    "detached subagent relay requires a persistent task-session root".to_string(),
                )
            })?;
        self.mutate_task_for_lease(id, |task| {
            validate_task_lease(task, lease, &owner_id)?;
            if lease.task_id() != id {
                return Err(TaskLeaseError::Fenced);
            }
            if task.task_type != TaskType::Subagent {
                return Err(TaskLeaseError::Persistence(
                    "subagent relay append requires a subagent task".to_string(),
                ));
            }
            if task
                .continuation_attempt_id
                .as_ref()
                .is_none_or(|expected| expected.as_str() != attempt_id)
            {
                return Err(TaskLeaseError::Fenced);
            }
            let relay_lease = RelayLease::new(
                id,
                task.task_type.into(),
                lease.owner_id(),
                lease.epoch(),
                attempt_id,
            )
            .map_err(TaskLeaseError::Relay)?;
            let relay =
                SubagentEventRelay::open(&root, relay_lease).map_err(TaskLeaseError::Relay)?;
            let result = relay.append(record).map_err(TaskLeaseError::Relay)?;
            // The mirror is deliberately updated only after the relay append
            // is durable.  A duplicate does not mutate the repairable mirror:
            // its delivery result is idempotent as well as the journal write.
            if matches!(
                result,
                crate::subagent_event_relay::AppendResult::Appended { .. }
            ) {
                task.last_activity_at_ms = Some(now_ms());
                task.publication_revision = task.publication_revision.saturating_add(1);
            }
            Ok(result)
        })
    }

    /// Opens a read-only relay for an attempt owned by a durable subagent task.
    /// The actor can drain this handle while the detached worker retains the
    /// task's write lease; no task snapshot or relay source bytes are mutated.
    #[allow(dead_code)] // Wired into the actor-owned relay drainer by Task 4.
    pub(crate) fn open_subagent_event_relay_reader(
        &self,
        id: &str,
        attempt_id: &str,
    ) -> Result<SubagentEventRelayReader, TaskLeaseError> {
        let root = self
            .persistence
            .as_ref()
            .map(|persistence| persistence.root.clone())
            .ok_or_else(|| {
                TaskLeaseError::Persistence(
                    "detached subagent relay requires a persistent task-session root".to_string(),
                )
            })?;
        let task = self.get(id).ok_or(TaskLeaseError::NotFound)?;
        if task.task_type != TaskType::Subagent {
            return Err(TaskLeaseError::Persistence(
                "subagent relay read requires a subagent task".to_string(),
            ));
        }
        let target = RelayReadTarget::new(id, task.task_type.into(), attempt_id)
            .map_err(TaskLeaseError::Relay)?;
        SubagentEventRelayReader::open(&root, target).map_err(TaskLeaseError::Relay)
    }

    pub fn complete_with_usage_and_lease(
        &self,
        lease: &TaskLease,
        id: &str,
        result: String,
        usage: Option<UsageTotals>,
    ) -> Result<(), TaskLeaseError> {
        self.update_task_with_lease(lease, id, |record| {
            record.status = TaskStatus::Completed;
            if record.started_at_ms.is_none() {
                record.started_at_ms = Some(now_ms());
            }
            record.completed_at_ms = Some(now_ms());
            record.result = Some(result);
            record.error = None;
            record.usage = usage;
            record.tool = None;
            record.pending_tool_call = None;
            record.pending_tool_approval_response = None;
            record.pending_provider_response = None;
            record.worker_pid = None;
            record.control.pause.store(false, Ordering::Release);
        })
    }

    pub fn fail_with_usage_and_lease(
        &self,
        lease: &TaskLease,
        id: &str,
        error: String,
        usage: Option<UsageTotals>,
    ) -> Result<(), TaskLeaseError> {
        self.update_task_with_lease(lease, id, |record| {
            record.status = TaskStatus::Failed;
            if record.started_at_ms.is_none() {
                record.started_at_ms = Some(now_ms());
            }
            record.completed_at_ms = Some(now_ms());
            record.error = Some(error);
            record.result = None;
            record.usage = usage;
            record.tool = None;
            record.pending_tool_call = None;
            record.pending_tool_approval_response = None;
            record.pending_provider_response = None;
            record.worker_pid = None;
            record.control.pause.store(false, Ordering::Release);
        })
    }

    fn update_task_with_lease<F>(
        &self,
        lease: &TaskLease,
        id: &str,
        update: F,
    ) -> Result<(), TaskLeaseError>
    where
        F: FnOnce(&mut TaskRecord),
    {
        let owner_id = self.owner_id.clone();
        self.mutate_task_for_lease(id, |record| {
            validate_task_lease(record, lease, &owner_id)?;
            if is_terminal(record.status) || record.stop_requested {
                return Err(TaskLeaseError::Fenced);
            }
            update(record);
            record.publication_revision = record.publication_revision.saturating_add(1);
            Ok(())
        })
    }

    fn mutate_task_for_lease<R, F>(&self, id: &str, mutate: F) -> Result<R, TaskLeaseError>
    where
        F: FnOnce(&mut TaskRecord) -> Result<R, TaskLeaseError>,
    {
        if let Some(persistence) = &self.persistence {
            let (result, record) = persistence.mutate_current_task(id, mutate)?;
            self.install_persisted_task(id, record)?;
            return Ok(result);
        }
        self.with_tasks(|tasks| {
            let record = tasks.get_mut(id).ok_or(TaskLeaseError::NotFound)?;
            mutate(record)
        })
        .map_err(|_| TaskLeaseError::Persistence("task registry lock poisoned".to_string()))?
    }

    fn install_persisted_task(
        &self,
        id: &str,
        mut record: TaskRecord,
    ) -> Result<(), TaskLeaseError> {
        self.with_tasks(|tasks| {
            if let Some(current) = tasks.get(id) {
                let cancel_requested =
                    record.stop_requested || record.control.cancel.is_cancelled();
                record.control = current.control.clone();
                if cancel_requested {
                    record.control.cancel.cancel();
                }
            }
            tasks.insert(id.to_string(), record);
        })
        .map_err(|_| TaskLeaseError::Persistence("task registry lock poisoned".to_string()))
    }

    pub(crate) fn record_typed_provider_outcome(
        &self,
        task_id: &str,
        outcome: DurableTypedProviderOutcome,
    ) -> Result<(), String> {
        let task = self
            .get(task_id)
            .ok_or_else(|| format!("task '{task_id}' not found"))?;
        if task.task_type != TaskType::MainSession {
            return Err("typed provider outcome requires a main session task".to_string());
        }
        let mut outcomes = self
            .typed_provider_outcomes
            .lock()
            .map_err(|_| "typed provider outcome lock poisoned".to_string())?;
        let previous = outcomes.insert(task_id.to_string(), outcome);
        if let Err(error) = self.persist_typed_provider_outcomes(&outcomes) {
            match previous {
                Some(previous) => {
                    outcomes.insert(task_id.to_string(), previous);
                }
                None => {
                    outcomes.remove(task_id);
                }
            }
            return Err(error);
        }
        Ok(())
    }

    pub(crate) fn typed_provider_outcome(
        &self,
        task_id: &str,
    ) -> Option<DurableTypedProviderOutcome> {
        self.typed_provider_outcomes
            .lock()
            .ok()
            .and_then(|outcomes| outcomes.get(task_id).cloned())
    }

    pub(crate) fn clear_typed_provider_outcome(&self, task_id: &str) -> Result<(), String> {
        let mut outcomes = self
            .typed_provider_outcomes
            .lock()
            .map_err(|_| "typed provider outcome lock poisoned".to_string())?;
        let previous = outcomes.remove(task_id);
        if let Err(error) = self.persist_typed_provider_outcomes(&outcomes) {
            if let Some(previous) = previous {
                outcomes.insert(task_id.to_string(), previous);
            }
            return Err(error);
        }
        Ok(())
    }

    pub fn create_workflow(
        &self,
        workflow_run_id: String,
        name: String,
        description: String,
        phase_count: usize,
    ) -> TaskHandle {
        let id = new_task_id();
        self.activate_prepared_workflow(
            id,
            workflow_run_id,
            name,
            description,
            phase_count,
            now_ms(),
        )
        .expect("task registry insert failed")
    }

    pub(crate) fn activate_prepared_workflow(
        &self,
        id: String,
        workflow_run_id: String,
        name: String,
        description: String,
        phase_count: usize,
        created_at_ms: i64,
    ) -> Result<TaskHandle, String> {
        let control = TaskControl {
            cancel: CancelToken::new(),
            pause: Arc::new(AtomicBool::new(false)),
            worker: Arc::new(Mutex::new(None)),
        };
        let record = TaskRecord {
            id: id.clone(),
            parent_task_id: None,
            task_type: TaskType::Workflow,
            status: TaskStatus::Queued,
            is_backgrounded: false,
            description,
            created_at_ms,
            started_at_ms: None,
            completed_at_ms: None,
            name: Some(name),
            agent_type: None,
            tool: None,
            pending_tool_call: None,
            pending_tool_approval_response: None,
            pending_provider_response: None,
            workflow_run_id: Some(workflow_run_id.clone()),
            phase_count: Some(phase_count),
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: None,
            continuation_id: None,
            continuation_attempt_id: None,
            continuation_checkpoint_id: None,
            continuation_revision: None,
            continuation_resumable: false,
            continuation_indeterminate: false,
            result: None,
            error: None,
            retry_count: 0,
            output_truncated: false,
            worker_pid: None,
            command: None,
            lease_owner: None,
            lease_epoch: 0,
            lease_expires_at_ms: None,
            stop_requested: false,
            publication_revision: 0,
            control,
        };

        self.with_tasks(|tasks| {
            if tasks.contains_key(&id) {
                return Err(format!("task '{id}' already exists"));
            }
            tasks.insert(id.clone(), record);
            self.persist_current_task(tasks, &id)
        })
        .map_err(|_| "task registry lock poisoned".to_string())??;

        Ok(TaskHandle {
            id,
            task_type: TaskType::Workflow,
            workflow_run_id: Some(workflow_run_id),
        })
    }

    pub fn create_subagent(&self, description: String, agent_type: Option<String>) -> TaskHandle {
        self.create_subagent_with_parent(description, agent_type, None)
    }

    pub fn create_subagent_with_parent(
        &self,
        description: String,
        agent_type: Option<String>,
        parent_task_id: Option<String>,
    ) -> TaskHandle {
        let mut ancestor_id = parent_task_id.clone();
        let mut refreshed = HashSet::new();
        while let Some(id) = ancestor_id {
            if !refreshed.insert(id.clone()) {
                break;
            }
            ancestor_id = self
                .refresh_task_from_persistence(&id)
                .expect("task registry parent refresh failed")
                .and_then(|record| record.parent_task_id);
        }
        let id = new_task_id();
        let created_at_ms = now_ms();
        let control = TaskControl {
            cancel: CancelToken::new(),
            pause: Arc::new(AtomicBool::new(false)),
            worker: Arc::new(Mutex::new(None)),
        };
        let record = TaskRecord {
            id: id.clone(),
            parent_task_id,
            task_type: TaskType::Subagent,
            status: TaskStatus::Queued,
            is_backgrounded: false,
            description,
            created_at_ms,
            started_at_ms: None,
            completed_at_ms: None,
            name: None,
            agent_type,
            tool: None,
            pending_tool_call: None,
            pending_tool_approval_response: None,
            pending_provider_response: None,
            workflow_run_id: None,
            phase_count: None,
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: None,
            continuation_id: None,
            continuation_attempt_id: None,
            continuation_checkpoint_id: None,
            continuation_revision: None,
            continuation_resumable: false,
            continuation_indeterminate: false,
            result: None,
            error: None,
            retry_count: 0,
            output_truncated: false,
            worker_pid: None,
            command: None,
            lease_owner: None,
            lease_epoch: 0,
            lease_expires_at_ms: None,
            stop_requested: false,
            publication_revision: 0,
            control,
        };

        let cancelled_roots = self
            .cancelled_roots
            .lock()
            .expect("cancelled task roots lock poisoned");
        let mut tasks = self.inner.lock().expect("task registry lock poisoned");
        let mut ancestor_id = record.parent_task_id.as_deref();
        let mut inspected = HashSet::new();
        let mut parent_cancelled = false;
        while let Some(id) = ancestor_id {
            if !inspected.insert(id.to_string()) {
                break;
            }
            if cancelled_roots.contains(id)
                || tasks.get(id).is_some_and(|ancestor| {
                    is_terminal(ancestor.status)
                        || ancestor.status == TaskStatus::Stopping
                        || ancestor.control.cancel.is_cancelled()
                })
            {
                parent_cancelled = true;
                break;
            }
            ancestor_id = tasks
                .get(id)
                .and_then(|ancestor| ancestor.parent_task_id.as_deref());
        }
        let mut record = record;
        if parent_cancelled {
            record.status = TaskStatus::Stopping;
            record.started_at_ms = Some(now_ms());
            record.control.cancel.cancel();
        }
        tasks.insert(id.clone(), record);
        self.persist_current_task(&tasks, &id)
            .expect("task registry insert failed");

        TaskHandle {
            id,
            task_type: TaskType::Subagent,
            workflow_run_id: None,
        }
    }

    pub fn create_main_session(&self, description: String) -> TaskHandle {
        let id = new_task_id();
        let created_at_ms = now_ms();
        let control = TaskControl {
            cancel: CancelToken::new(),
            pause: Arc::new(AtomicBool::new(false)),
            worker: Arc::new(Mutex::new(None)),
        };
        let record = TaskRecord {
            id: id.clone(),
            parent_task_id: None,
            task_type: TaskType::MainSession,
            status: TaskStatus::Queued,
            is_backgrounded: false,
            description,
            created_at_ms,
            started_at_ms: None,
            completed_at_ms: None,
            name: None,
            agent_type: Some("main-session".to_string()),
            tool: None,
            pending_tool_call: None,
            pending_tool_approval_response: None,
            pending_provider_response: None,
            workflow_run_id: None,
            phase_count: None,
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: None,
            continuation_id: None,
            continuation_attempt_id: None,
            continuation_checkpoint_id: None,
            continuation_revision: None,
            continuation_resumable: false,
            continuation_indeterminate: false,
            result: None,
            error: None,
            retry_count: 0,
            output_truncated: false,
            worker_pid: None,
            command: None,
            lease_owner: None,
            lease_epoch: 0,
            lease_expires_at_ms: None,
            stop_requested: false,
            publication_revision: 0,
            control,
        };

        self.insert_task(id.clone(), record)
            .expect("task registry insert failed");

        TaskHandle {
            id,
            task_type: TaskType::MainSession,
            workflow_run_id: None,
        }
    }

    pub fn create_shell(&self, description: String, command: String) -> TaskHandle {
        let id = new_task_id();
        let created_at_ms = now_ms();
        let control = TaskControl {
            cancel: CancelToken::new(),
            pause: Arc::new(AtomicBool::new(false)),
            worker: Arc::new(Mutex::new(None)),
        };
        let record = TaskRecord {
            id: id.clone(),
            parent_task_id: None,
            task_type: TaskType::Shell,
            status: TaskStatus::Queued,
            is_backgrounded: false,
            description,
            created_at_ms,
            started_at_ms: None,
            completed_at_ms: None,
            name: None,
            agent_type: None,
            tool: None,
            pending_tool_call: None,
            pending_tool_approval_response: None,
            pending_provider_response: None,
            workflow_run_id: None,
            phase_count: None,
            workflow_progress: None,
            workflow_phases: Vec::new(),
            workflow_agents: Vec::new(),
            workflow_script_path: None,
            workflow_launch_input: None,
            workflow_final_summary: None,
            workflow_failure_count: 0,
            usage: None,
            subagent_current_activity: None,
            subagent_turn: None,
            last_activity_at_ms: None,
            continuation_id: None,
            continuation_attempt_id: None,
            continuation_checkpoint_id: None,
            continuation_revision: None,
            continuation_resumable: false,
            continuation_indeterminate: false,
            result: None,
            error: None,
            retry_count: 0,
            output_truncated: false,
            worker_pid: None,
            command: Some(command),
            lease_owner: None,
            lease_epoch: 0,
            lease_expires_at_ms: None,
            stop_requested: false,
            publication_revision: 0,
            control,
        };

        self.insert_task(id.clone(), record)
            .expect("task registry insert failed");

        TaskHandle {
            id,
            task_type: TaskType::Shell,
            workflow_run_id: None,
        }
    }

    pub fn list(&self) -> Vec<BackgroundTaskSummary> {
        let _ = self.refresh_session_from_persistence();
        let mut summaries = self
            .with_tasks(|tasks| tasks.values().map(task_summary).collect::<Vec<_>>())
            .expect("task registry lock poisoned");
        summaries.sort_by(|left, right| left.id.cmp(&right.id));
        summaries
    }

    pub fn has_active_tasks(&self) -> bool {
        self.activity_summary().has_active_tasks()
    }

    pub fn requires_attention(&self) -> bool {
        self.activity_summary().requires_attention()
    }

    pub fn activity_summary(&self) -> TaskActivitySummary {
        TaskActivitySummary::from_tasks(&self.list())
    }

    pub fn summary(&self, id: &str) -> Option<BackgroundTaskSummary> {
        if self.persistence.is_some() {
            return self
                .refresh_task_from_persistence(id)
                .ok()
                .flatten()
                .map(|record| task_summary(&record));
        }
        self.get(id).map(|record| task_summary(&record))
    }

    pub fn get(&self, id: &str) -> Option<TaskRecord> {
        if let Some(record) = self
            .with_tasks(|tasks| tasks.get(id).cloned())
            .expect("task registry lock poisoned")
        {
            return Some(record);
        }

        let persistence = self.persistence.as_ref()?;
        let record = persistence
            .load_record_by_id(id, self.recover_persisted_active_tasks, &self.session_id)
            .ok()??;
        self.with_tasks(|tasks| {
            tasks.insert(id.to_string(), record.clone());
        })
        .expect("task registry lock poisoned");
        Some(record)
    }

    pub fn update_workflow_progress(
        &self,
        id: &str,
        progress: WorkflowTaskProgress,
    ) -> Result<(), String> {
        self.update_task(id, |record| {
            record.workflow_progress = Some(progress);
            Ok(())
        })
    }

    pub fn update_workflow_agents(
        &self,
        id: &str,
        agents: Vec<WorkflowAgentTaskSummary>,
    ) -> Result<(), String> {
        self.update_task(id, |record| {
            record.workflow_agents = agents;
            Ok(())
        })
    }

    pub fn update_workflow_phases(
        &self,
        id: &str,
        phases: Vec<WorkflowPhaseTaskSummary>,
    ) -> Result<(), String> {
        self.update_task(id, |record| {
            record.workflow_phases = phases;
            Ok(())
        })
    }

    pub fn update_workflow_artifacts(
        &self,
        id: &str,
        script_path: String,
        launch_input: WorkflowInput,
    ) -> Result<(), String> {
        self.update_task(id, |record| {
            record.workflow_script_path = Some(script_path);
            record.workflow_launch_input = Some(launch_input);
            Ok(())
        })
    }

    pub fn update_workflow_result_summary(
        &self,
        id: &str,
        final_summary: Option<String>,
        failure_count: u32,
    ) -> Result<(), String> {
        self.update_task(id, |record| {
            record.workflow_final_summary = final_summary;
            record.workflow_failure_count = failure_count;
            Ok(())
        })
    }

    pub fn update_subagent_activity(
        &self,
        id: &str,
        activity: String,
        turn: Option<u32>,
        usage: Option<UsageTotals>,
    ) -> Result<(), String> {
        self.update_task(id, |record| {
            if record.task_type != TaskType::Subagent {
                return Err(format!("task '{id}' is not a subagent"));
            }
            record.subagent_current_activity = Some(activity);
            if let Some(turn) = turn {
                record.subagent_turn = Some(turn);
            }
            if let Some(usage) = usage {
                record.usage = Some(usage);
            }
            record.last_activity_at_ms = Some(now_ms());
            Ok(())
        })
    }

    pub fn mark_running(&self, id: &str) -> Result<(), String> {
        self.update_task(id, |record| {
            if is_terminal(record.status) || record.control.cancel.is_cancelled() {
                return Err(task_state_error("mark_running", record.status));
            }

            record.status = TaskStatus::Running;
            if record.started_at_ms.is_none() {
                record.started_at_ms = Some(now_ms());
            }
            record.completed_at_ms = None;
            record.control.pause.store(false, Ordering::Release);
            Ok(())
        })
    }

    pub fn record_retry(&self, id: &str, error: impl Into<String>) -> Result<(), String> {
        let error = error.into();
        self.update_task(id, |record| {
            if is_terminal(record.status)
                && !matches!(
                    record.status,
                    TaskStatus::Failed | TaskStatus::ApprovalRequired
                )
            {
                return Err(task_state_error("record_retry", record.status));
            }
            record.retry_count = record.retry_count.saturating_add(1);
            record.status = TaskStatus::Running;
            record.error = Some(error.clone());
            record.result = None;
            record.completed_at_ms = None;
            record.last_activity_at_ms = Some(now_ms());
            Ok(())
        })
    }

    pub fn mark_output_truncated(&self, id: &str) -> Result<(), String> {
        self.update_task(id, |record| {
            record.output_truncated = true;
            record.last_activity_at_ms = Some(now_ms());
            Ok(())
        })
    }

    pub fn mark_backgrounded(&self, id: &str) -> Result<(), String> {
        self.update_task(id, |record| {
            if record.task_type != TaskType::MainSession {
                return Err("mark_backgrounded requires a main session task".to_string());
            }
            if record.status != TaskStatus::Running {
                return Err(task_state_error("mark_backgrounded", record.status));
            }

            record.is_backgrounded = true;
            record.last_activity_at_ms = Some(now_ms());
            Ok(())
        })
    }

    pub fn mark_foregrounded(&self, id: &str) -> Result<(), String> {
        self.update_task(id, |record| {
            if record.task_type != TaskType::MainSession {
                return Err("mark_foregrounded requires a main session task".to_string());
            }
            if record.status != TaskStatus::Running {
                return Err(task_state_error("mark_foregrounded", record.status));
            }
            if !record.is_backgrounded {
                return Err("mark_foregrounded requires a backgrounded task".to_string());
            }

            record.is_backgrounded = false;
            record.last_activity_at_ms = Some(now_ms());
            Ok(())
        })
    }

    pub(crate) fn reconcile_main_session_backgrounded(
        &self,
        id: &str,
        is_backgrounded: bool,
    ) -> Result<(), String> {
        self.update_task(id, |record| {
            if record.task_type != TaskType::MainSession {
                return Err(
                    "reconcile_main_session_backgrounded requires a main session task".to_string(),
                );
            }
            record.is_backgrounded = is_backgrounded;
            Ok(())
        })
    }

    pub fn apply_main_session_terminal_update(
        &self,
        id: &str,
        update: MainSessionTerminalUpdate,
        usage: Option<UsageTotals>,
    ) -> Result<TaskTerminalTransition, String> {
        self.mutate_task(id, |record| {
            if record.task_type != TaskType::MainSession {
                return Err(
                    "apply_main_session_terminal_update requires a main session task".to_string(),
                );
            }
            if is_terminal(record.status) {
                return Err(task_state_error(
                    "apply_main_session_terminal_update",
                    record.status,
                ));
            }

            if record.stop_requested || record.control.cancel.is_cancelled() {
                record.status = TaskStatus::Stopped;
                record.result = Some("cancelled".to_string());
                record.error = None;
                record.tool = None;
                record.pending_tool_call = None;
                record.pending_tool_approval_response = None;
                record.pending_provider_response = None;
            } else {
                match update {
                    MainSessionTerminalUpdate::Completed { result } => {
                        record.status = TaskStatus::Completed;
                        record.result = Some(result);
                        record.error = None;
                        record.tool = None;
                        record.pending_tool_call = None;
                        record.pending_tool_approval_response = None;
                        record.pending_provider_response = None;
                    }
                    MainSessionTerminalUpdate::Failed { error } => {
                        record.status = TaskStatus::Failed;
                        record.result = None;
                        record.error = Some(error);
                        record.tool = None;
                        record.pending_tool_call = None;
                        record.pending_tool_approval_response = None;
                        record.pending_provider_response = None;
                    }
                    MainSessionTerminalUpdate::ApprovalRequired {
                        summary,
                        pending_tool_call,
                        pending_provider_response,
                    } => {
                        record.status = TaskStatus::ApprovalRequired;
                        record.result = Some(summary);
                        record.error = None;
                        record.tool = pending_tool_call
                            .as_ref()
                            .map(|pending_tool_call| pending_tool_call.name.clone());
                        record.pending_tool_call = pending_tool_call;
                        record.pending_tool_approval_response = None;
                        record.pending_provider_response = pending_provider_response;
                    }
                }
            }
            if record.started_at_ms.is_none() {
                record.started_at_ms = Some(now_ms());
            }
            record.completed_at_ms = Some(now_ms());
            record.usage = usage;
            record.worker_pid = None;
            record.control.pause.store(false, Ordering::Release);
            let transition = TaskTerminalTransition {
                is_backgrounded: record.is_backgrounded,
            };
            Ok((transition, true))
        })
    }

    pub fn mark_worker_spawned(&self, id: &str, pid: u32) -> Result<(), String> {
        self.update_task(id, |record| {
            if is_terminal(record.status) {
                return Err(task_state_error("mark_worker_spawned", record.status));
            }
            record.worker_pid = Some(pid);
            Ok(())
        })
    }

    #[cfg(test)]
    pub(crate) fn adopt_subagent_worker(&self, id: &str, child: Child) -> Result<(), String> {
        let pid = child.id();
        #[cfg(windows)]
        let process_job = ProcessJob::attach_named(pid, &async_worker_job_name(id));
        #[cfg(not(windows))]
        let process_job = ProcessJob::attach(pid);
        let process_job = match process_job {
            Ok(process_job) => process_job,
            Err(error) => {
                let mut child = child;
                orca_tools::process::kill_child_tree(&mut child);
                let _ = child.wait();
                return Err(format!(
                    "failed to assign async subagent process job: {error}"
                ));
            }
        };
        self.adopt_subagent_worker_with_job(id, child, process_job)
    }

    pub(crate) fn adopt_subagent_worker_with_job(
        &self,
        id: &str,
        child: Child,
        process_job: ProcessJob,
    ) -> Result<(), String> {
        let pid = child.id();
        let mut owned_worker = Some(OwnedWorker { child, process_job });
        let worker = self
            .with_tasks(|tasks| {
                let worker = {
                    let record = tasks
                        .get(id)
                        .ok_or_else(|| format!("task '{id}' not found"))?;
                    if record.task_type != TaskType::Subagent {
                        return Err(format!("task '{id}' is not a subagent"));
                    }
                    if is_terminal(record.status) {
                        return Err(task_state_error("adopt_subagent_worker", record.status));
                    }
                    Arc::clone(&record.control.worker)
                };
                let mut slot = worker
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if slot.is_some() {
                    return Err(format!("task '{id}' already owns a worker process"));
                }
                let previous_pid = tasks.get(id).and_then(|record| record.worker_pid);
                let previous_status = tasks.get(id).expect("validated task record").status;
                let previous_started_at =
                    tasks.get(id).expect("validated task record").started_at_ms;
                let previous_completed_at = tasks
                    .get(id)
                    .expect("validated task record")
                    .completed_at_ms;
                let previous_publication_revision = tasks
                    .get(id)
                    .expect("validated task record")
                    .publication_revision;
                let record = tasks.get_mut(id).expect("validated task record");
                record.worker_pid = Some(pid);
                record.status = TaskStatus::Running;
                if record.started_at_ms.is_none() {
                    record.started_at_ms = Some(now_ms());
                }
                record.completed_at_ms = None;
                record.publication_revision = record.publication_revision.saturating_add(1);
                let adoption_revision = record.publication_revision;
                if let Err(error) = self.persist_current_task(tasks, id) {
                    let record = tasks.get_mut(id).expect("validated task record");
                    record.worker_pid = previous_pid;
                    record.status = previous_status;
                    record.started_at_ms = previous_started_at;
                    record.completed_at_ms = previous_completed_at;
                    record.publication_revision = previous_publication_revision;
                    return Err(error);
                }
                *slot = owned_worker.take();
                drop(slot);
                Ok((worker, adoption_revision))
            })
            .map_err(|_| "task registry lock poisoned".to_string())?;

        match worker {
            Ok((worker, adoption_revision)) => {
                spawn_worker_reaper(self.clone(), id.to_string(), pid, adoption_revision, worker);
                Ok(())
            }
            Err(error) => {
                if let Some(mut owned_worker) = owned_worker {
                    terminate_worker(&mut owned_worker);
                    let _ = owned_worker.child.wait();
                }
                Err(error)
            }
        }
    }

    pub fn complete(&self, id: &str, result: String) -> Result<(), String> {
        self.complete_with_usage(id, result, None)
    }

    pub fn complete_with_usage(
        &self,
        id: &str,
        result: String,
        usage: Option<UsageTotals>,
    ) -> Result<(), String> {
        self.update_task(id, |record| {
            record.status = TaskStatus::Completed;
            if record.started_at_ms.is_none() {
                record.started_at_ms = Some(now_ms());
            }
            record.completed_at_ms = Some(now_ms());
            record.result = Some(result);
            record.error = None;
            record.usage = usage;
            record.tool = None;
            record.pending_tool_call = None;
            record.pending_tool_approval_response = None;
            record.pending_provider_response = None;
            record.worker_pid = None;
            record.control.pause.store(false, Ordering::Release);
            Ok(())
        })
    }

    pub fn fail(&self, id: &str, error: String) -> Result<(), String> {
        self.fail_with_usage(id, error, None)
    }

    pub fn fail_with_usage(
        &self,
        id: &str,
        error: String,
        usage: Option<UsageTotals>,
    ) -> Result<(), String> {
        self.update_task(id, |record| {
            record.status = TaskStatus::Failed;
            if record.started_at_ms.is_none() {
                record.started_at_ms = Some(now_ms());
            }
            record.completed_at_ms = Some(now_ms());
            record.error = Some(error);
            record.result = None;
            record.usage = usage;
            record.tool = None;
            record.pending_tool_call = None;
            record.pending_tool_approval_response = None;
            record.pending_provider_response = None;
            record.worker_pid = None;
            record.control.pause.store(false, Ordering::Release);
            Ok(())
        })
    }

    pub fn approval_required(&self, id: &str, summary: String) -> Result<(), String> {
        self.approval_required_for_tool(id, summary, None)
    }

    pub fn approval_required_for_tool(
        &self,
        id: &str,
        summary: String,
        tool: Option<String>,
    ) -> Result<(), String> {
        self.approval_required_with_pending_tool(id, summary, tool, None)
    }

    pub fn approval_required_for_pending_tool(
        &self,
        id: &str,
        summary: String,
        pending_tool_call: Option<PendingToolCallSummary>,
    ) -> Result<(), String> {
        let tool = pending_tool_call
            .as_ref()
            .map(|pending_tool_call| pending_tool_call.name.clone());
        self.approval_required_with_pending_tool(id, summary, tool, pending_tool_call)
    }

    pub fn approval_required_for_pending_provider_response(
        &self,
        id: &str,
        summary: String,
        response: RuntimeModelResponse,
    ) -> Result<(), String> {
        self.approval_required_for_pending_provider_response_with_usage(id, summary, response, None)
    }

    pub fn approval_required_for_pending_provider_response_with_usage(
        &self,
        id: &str,
        summary: String,
        response: RuntimeModelResponse,
        usage: Option<UsageTotals>,
    ) -> Result<(), String> {
        let pending_tool_call = pending_tool_call_from_provider_response(&response.response);
        let tool = pending_tool_call
            .as_ref()
            .map(|pending_tool_call| pending_tool_call.name.clone());
        self.approval_required_with_pending_provider_response(
            id,
            summary,
            tool,
            pending_tool_call,
            Some(response),
            usage,
        )
    }

    fn approval_required_with_pending_tool(
        &self,
        id: &str,
        summary: String,
        tool: Option<String>,
        pending_tool_call: Option<PendingToolCallSummary>,
    ) -> Result<(), String> {
        self.approval_required_with_pending_provider_response(
            id,
            summary,
            tool,
            pending_tool_call,
            None,
            None,
        )
    }

    fn approval_required_with_pending_provider_response(
        &self,
        id: &str,
        summary: String,
        tool: Option<String>,
        pending_tool_call: Option<PendingToolCallSummary>,
        pending_provider_response: Option<RuntimeModelResponse>,
        usage: Option<UsageTotals>,
    ) -> Result<(), String> {
        self.update_task(id, |record| {
            record.status = TaskStatus::ApprovalRequired;
            if record.started_at_ms.is_none() {
                record.started_at_ms = Some(now_ms());
            }
            record.completed_at_ms = Some(now_ms());
            record.result = Some(summary);
            record.error = None;
            record.tool = tool;
            record.pending_tool_call = pending_tool_call;
            record.pending_tool_approval_response = None;
            record.pending_provider_response = pending_provider_response;
            record.usage = usage;
            record.worker_pid = None;
            record.control.pause.store(false, Ordering::Release);
            Ok(())
        })
    }

    pub fn submit_pending_tool_approval_response(
        &self,
        id: &str,
        approved: bool,
    ) -> Result<(), String> {
        self.update_task(id, |record| {
            if record.status != TaskStatus::ApprovalRequired || record.pending_tool_call.is_none() {
                return Err(format!(
                    "cannot submit approval response without pending approval_required tool for task '{}'",
                    record.id
                ));
            }
            record.pending_tool_approval_response = Some(approved);
            Ok(())
        })
    }

    pub fn submit_pending_tool_approval_response_by_request_id(
        &self,
        request_id: &str,
        approved: bool,
    ) -> Result<String, String> {
        self.refresh_session_from_persistence()?;
        let task_id = self
            .with_tasks(|tasks| {
                let mut matching_task_ids = tasks
                    .iter()
                    .filter(|(_, record)| {
                        record.status == TaskStatus::ApprovalRequired
                            && record
                                .pending_tool_call
                                .as_ref()
                                .is_some_and(|pending_tool_call| pending_tool_call.id == request_id)
                    })
                    .map(|(task_id, _)| task_id.clone());
                let Some(task_id) = matching_task_ids.next() else {
                    return Err(format!("pending approval request '{request_id}' not found"));
                };
                if matching_task_ids.next().is_some() {
                    return Err(format!(
                        "pending approval request '{request_id}' matched multiple tasks"
                    ));
                }
                Ok(task_id)
            })
            .map_err(|_| "task registry lock poisoned".to_string())??;
        self.update_task(&task_id, |record| {
            if record.pending_tool_approval_response.is_some() {
                return Err(format!(
                    "pending approval request '{request_id}' already has a response"
                ));
            }
            record.pending_tool_approval_response = Some(approved);
            Ok(())
        })?;
        Ok(task_id)
    }

    pub fn take_pending_tool_approval_response(&self, id: &str) -> Result<Option<bool>, String> {
        self.mutate_task(id, |record| {
            let response = record.pending_tool_approval_response.take();
            Ok((response, response.is_some()))
        })
    }

    pub fn take_approved_pending_provider_response(
        &self,
        id: &str,
    ) -> Result<Option<RuntimeModelResponse>, String> {
        self.mutate_task(id, |record| {
            if record.status != TaskStatus::ApprovalRequired
                || record.pending_tool_approval_response != Some(true)
            {
                return Ok((None, false));
            }

            let Some(response) = record.pending_provider_response.take() else {
                return Ok((None, false));
            };

            record.status = TaskStatus::Running;
            if record.started_at_ms.is_none() {
                record.started_at_ms = Some(now_ms());
            }
            record.completed_at_ms = None;
            record.result = None;
            record.error = None;
            record.tool = None;
            record.pending_tool_call = None;
            record.pending_tool_approval_response = None;
            record.worker_pid = None;
            record.last_activity_at_ms = Some(now_ms());
            record.control.pause.store(false, Ordering::Release);
            Ok((Some(response), true))
        })
    }

    pub fn finish_denied_pending_tool_approval(&self, id: &str) -> Result<bool, String> {
        let mut consumed = false;
        self.update_task(id, |record| {
            if record.status != TaskStatus::ApprovalRequired
                || record.pending_tool_call.is_none()
                || record.pending_tool_approval_response != Some(false)
            {
                return Ok(());
            }

            record.status = TaskStatus::Stopped;
            if record.started_at_ms.is_none() {
                record.started_at_ms = Some(now_ms());
            }
            record.completed_at_ms = Some(now_ms());
            record.result = Some("Approval denied".to_string());
            record.error = None;
            record.tool = None;
            record.pending_tool_call = None;
            record.pending_tool_approval_response = None;
            record.pending_provider_response = None;
            record.worker_pid = None;
            record.control.pause.store(false, Ordering::Release);
            consumed = true;
            Ok(())
        })?;
        Ok(consumed)
    }

    pub fn stop(&self, id: &str, summary: String) -> Result<(), String> {
        self.stop_with_usage(id, summary, None)
    }

    pub fn stop_with_usage(
        &self,
        id: &str,
        summary: String,
        usage: Option<UsageTotals>,
    ) -> Result<(), String> {
        self.update_task(id, |record| {
            record.status = TaskStatus::Stopped;
            if record.started_at_ms.is_none() {
                record.started_at_ms = Some(now_ms());
            }
            record.completed_at_ms = Some(now_ms());
            record.result = Some(summary);
            record.error = None;
            record.tool = None;
            record.pending_tool_call = None;
            record.pending_tool_approval_response = None;
            record.pending_provider_response = None;
            record.worker_pid = None;
            record.control.pause.store(false, Ordering::Release);
            if usage.is_some() {
                record.usage = usage;
            }
            Ok(())
        })
    }

    pub fn request_stop(&self, id: &str) -> Result<(), String> {
        let stopped_record = self.mark_stop_requested_record(id)?;
        let target = self
            .with_tasks(|tasks| {
                let Some(pid) = (stopped_record.task_type == TaskType::Subagent)
                    .then_some(stopped_record.worker_pid)
                    .flatten()
                else {
                    return Ok(TaskStopTarget::InProcess);
                };
                let worker = tasks
                    .get(id)
                    .map(|record| Arc::clone(&record.control.worker))
                    .ok_or_else(|| format!("task '{id}' not found"))?;
                let agent_id = stopped_record.id.clone();
                let mut slot = worker
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(owned_worker) = slot.take() {
                    if owned_worker.child.id() == pid {
                        drop(slot);
                        return Ok(TaskStopTarget::Owned {
                            worker,
                            owned_worker,
                        });
                    }
                    *slot = Some(owned_worker);
                }
                drop(slot);
                if pid == 0 {
                    return Err(format!("task '{id}' worker has not finished starting"));
                }
                Ok(TaskStopTarget::Recovered { pid, agent_id })
            })
            .map_err(|_| "task registry lock poisoned".to_string())??;

        if let TaskStopTarget::Recovered { pid, agent_id } = &target {
            match verify_recovered_worker(*pid, agent_id)? {
                RecoveredWorkerState::Missing | RecoveredWorkerState::Replaced => {
                    return self.stop(id, "Task stopped; worker already exited".to_string());
                }
                RecoveredWorkerState::Matches => {}
            }
        }

        match target {
            TaskStopTarget::InProcess => Ok(()),
            TaskStopTarget::Owned {
                worker,
                mut owned_worker,
            } => {
                terminate_worker(&mut owned_worker);
                if let Err(error) = owned_worker.child.wait() {
                    let mut slot = worker
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    *slot = Some(owned_worker);
                    return Err(format!("failed to reap async subagent worker: {error}"));
                }
                self.stop(id, "Task stopped".to_string())
            }
            TaskStopTarget::Recovered { pid, agent_id } => {
                terminate_recovered_worker(pid, &agent_id)?;
                self.stop(id, "Task stopped".to_string())
            }
        }
    }

    pub fn request_stop_tree(&self, root_id: &str) -> Result<Vec<String>, String> {
        self.refresh_session_from_persistence()?;
        let root = self.get(root_id);
        let root_active = root.as_ref().is_some_and(|root| !is_terminal(root.status));
        if root.is_none()
            && !self
                .cancelled_roots
                .lock()
                .map_err(|_| "cancelled task roots lock poisoned".to_string())?
                .contains(root_id)
        {
            return Err(format!("task '{root_id}' not found"));
        }
        let mut stopped = Vec::new();
        if root_active {
            match self.request_stop(root_id) {
                Ok(()) => stopped.push(root_id.to_string()),
                Err(_)
                    if self
                        .get(root_id)
                        .is_some_and(|task| is_terminal(task.status)) => {}
                Err(error) => return Err(error),
            }
        }
        let mut seen = stopped.iter().cloned().collect::<HashSet<_>>();
        loop {
            self.refresh_session_from_persistence()?;
            let mut targets = self
                .with_tasks(|tasks| {
                    let mut targets = tasks
                        .values()
                        .filter(|record| record.id != root_id)
                        .filter(|record| !seen.contains(&record.id))
                        .filter_map(|record| {
                            (!is_terminal(record.status))
                                .then(|| task_depth_below(tasks, &record.id, root_id))
                                .flatten()
                                .map(|depth| (depth, record.id.clone()))
                        })
                        .collect::<Vec<_>>();
                    targets.sort_by(|left, right| {
                        left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1))
                    });
                    targets
                })
                .map_err(|_| "task registry lock poisoned".to_string())?;
            if targets.is_empty() {
                break;
            }
            for (_, task_id) in targets.drain(..) {
                seen.insert(task_id.clone());
                match self.request_stop(&task_id) {
                    Ok(()) => stopped.push(task_id),
                    Err(_)
                        if self
                            .get(&task_id)
                            .is_some_and(|task| is_terminal(task.status)) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(stopped)
    }

    pub fn signal_stop_tree(&self, root_id: &str) -> Result<Vec<String>, String> {
        self.cancelled_roots
            .lock()
            .map_err(|_| "cancelled task roots lock poisoned".to_string())?
            .insert(root_id.to_string());
        let mut targets = self
            .with_tasks(|tasks| {
                let mut targets = tasks
                    .values()
                    .filter_map(|record| {
                        (!is_terminal(record.status))
                            .then(|| task_depth_below(tasks, &record.id, root_id))
                            .flatten()
                            .map(|depth| (depth, record.id.clone()))
                    })
                    .collect::<Vec<_>>();
                targets
                    .sort_by(|left, right| right.0.cmp(&left.0).then_with(|| left.1.cmp(&right.1)));
                targets
            })
            .map_err(|_| "task registry lock poisoned".to_string())?;
        let mut signalled = Vec::with_capacity(targets.len());
        for (_, task_id) in targets.drain(..) {
            self.mark_stop_requested(&task_id)?;
            signalled.push(task_id);
        }
        Ok(signalled)
    }

    fn mark_stop_requested(&self, id: &str) -> Result<(), String> {
        self.mark_stop_requested_record(id).map(|_| ())
    }

    fn mark_stop_requested_record(&self, id: &str) -> Result<TaskRecord, String> {
        self.mutate_task(id, |record| {
            if is_terminal(record.status) {
                return Err(task_state_error("request_stop", record.status));
            }
            record.status = TaskStatus::Stopping;
            record.stop_requested = true;
            record.lease_epoch = record.lease_epoch.saturating_add(1);
            record.lease_owner = None;
            record.lease_expires_at_ms = None;
            if record.started_at_ms.is_none() {
                record.started_at_ms = Some(now_ms());
            }
            record.control.cancel.cancel();
            Ok((record.clone(), true))
        })
    }

    pub fn request_pause(&self, id: &str) -> Result<(), String> {
        self.update_task(id, |record| {
            if is_terminal(record.status) {
                return Err(task_state_error("request_pause", record.status));
            }

            record.status = TaskStatus::Paused;
            if record.started_at_ms.is_none() {
                record.started_at_ms = Some(now_ms());
            }
            record.control.pause.store(true, Ordering::Release);
            Ok(())
        })
    }

    pub fn request_resume(&self, id: &str) -> Result<(), String> {
        self.update_task(id, |record| {
            if is_terminal(record.status) || record.control.cancel.is_cancelled() {
                return Err(task_state_error("request_resume", record.status));
            }

            record.status = TaskStatus::Running;
            if record.started_at_ms.is_none() {
                record.started_at_ms = Some(now_ms());
            }
            record.completed_at_ms = None;
            record.control.pause.store(false, Ordering::Release);
            Ok(())
        })
    }

    pub fn is_cancelled(&self, id: &str) -> bool {
        self.with_tasks(|tasks| {
            tasks
                .get(id)
                .is_some_and(|record| record.control.cancel.is_cancelled())
        })
        .unwrap_or(false)
    }

    fn mutate_task<R, F>(&self, id: &str, mutate: F) -> Result<R, String>
    where
        F: FnOnce(&mut TaskRecord) -> Result<(R, bool), String>,
    {
        if let Some(persistence) = &self.persistence {
            let (result, record) = persistence
                .mutate_current_task(id, |record| {
                    let (result, changed) = mutate(record).map_err(TaskLeaseError::Persistence)?;
                    if changed {
                        record.publication_revision = record.publication_revision.saturating_add(1);
                    }
                    Ok(result)
                })
                .map_err(|error| error.to_string())?;
            self.install_persisted_task(id, record)
                .map_err(|error| error.to_string())?;
            return Ok(result);
        }
        self.with_tasks(|tasks| {
            let record = tasks
                .get_mut(id)
                .ok_or_else(|| format!("task '{id}' not found"))?;
            let (result, changed) = mutate(record)?;
            if changed {
                record.publication_revision = record.publication_revision.saturating_add(1);
                self.persist_current_task(tasks, id)?;
            }
            Ok(result)
        })
        .map_err(|_| "task registry lock poisoned".to_string())?
    }

    fn update_task<F>(&self, id: &str, update: F) -> Result<(), String>
    where
        F: FnOnce(&mut TaskRecord) -> Result<(), String>,
    {
        self.mutate_task(id, |record| update(record).map(|()| ((), true)))
    }

    fn insert_task(&self, id: String, record: TaskRecord) -> Result<(), String> {
        self.with_tasks(|tasks| {
            tasks.insert(id.clone(), record);
            self.persist_current_task(tasks, &id)
        })
        .map_err(|_| "task registry lock poisoned".to_string())?
    }

    fn refresh_task_from_persistence(&self, id: &str) -> Result<Option<TaskRecord>, String> {
        let Some(persistence) = self.persistence.as_ref() else {
            return Ok(self.get(id));
        };
        let Some(record) = persistence
            .load_record_by_id(id, self.recover_persisted_active_tasks, &self.session_id)
            .map_err(|error| error.to_string())?
        else {
            return Ok(None);
        };
        self.install_persisted_task(id, record)
            .map_err(|error| error.to_string())?;
        self.with_tasks(|tasks| tasks.get(id).cloned())
            .map_err(|_| "task registry lock poisoned".to_string())
    }

    fn worker_exit_fence(
        &self,
        id: &str,
        worker_pid: u32,
        adoption_revision: u64,
    ) -> Result<Option<WorkerExitFence>, String> {
        let record = if self.persistence.is_some() {
            self.refresh_task_from_persistence(id)?
        } else {
            self.get(id)
        };
        let Some(record) = record else {
            return Ok(None);
        };
        if is_terminal(record.status) || record.worker_pid != Some(worker_pid) {
            return Ok(None);
        }
        if let Some(lease_owner) = record.lease_owner.clone() {
            if record
                .lease_expires_at_ms
                .is_none_or(|expires_at_ms| expires_at_ms <= now_ms())
                || task_lease_owner_pid(&lease_owner) != Some(worker_pid)
            {
                return Ok(None);
            }
            return Ok(Some(WorkerExitFence {
                worker_pid,
                adoption_revision,
                lease_owner: Some(lease_owner),
                lease_epoch: record.lease_epoch,
            }));
        }
        if record.publication_revision != adoption_revision {
            return Ok(None);
        }
        Ok(Some(WorkerExitFence {
            worker_pid,
            adoption_revision,
            lease_owner: None,
            lease_epoch: record.lease_epoch,
        }))
    }

    fn fail_worker_exit_if_current(
        &self,
        id: &str,
        fence: &WorkerExitFence,
    ) -> Result<bool, String> {
        self.mutate_task(id, |record| {
            if is_terminal(record.status) || record.worker_pid != Some(fence.worker_pid) {
                return Ok((false, false));
            }
            let lease_is_current = match fence.lease_owner.as_deref() {
                Some(lease_owner) => {
                    record.lease_owner.as_deref() == Some(lease_owner)
                        && record.lease_epoch == fence.lease_epoch
                        && record
                            .lease_expires_at_ms
                            .is_some_and(|expires_at_ms| expires_at_ms > now_ms())
                }
                None => {
                    record.lease_owner.is_none()
                        && record.lease_epoch == fence.lease_epoch
                        && record.publication_revision == fence.adoption_revision
                }
            };
            if !lease_is_current {
                return Ok((false, false));
            }
            record.status = TaskStatus::Failed;
            if record.started_at_ms.is_none() {
                record.started_at_ms = Some(now_ms());
            }
            record.completed_at_ms = Some(now_ms());
            record.error =
                Some("async subagent worker exited without a terminal task update".to_string());
            record.result = None;
            record.tool = None;
            record.pending_tool_call = None;
            record.pending_tool_approval_response = None;
            record.pending_provider_response = None;
            record.worker_pid = None;
            record.control.pause.store(false, Ordering::Release);
            Ok((true, true))
        })
    }

    fn refresh_session_from_persistence(&self) -> Result<(), String> {
        let Some(persistence) = self.persistence.as_ref() else {
            return Ok(());
        };
        let records = persistence
            .load_session_records(&self.session_id)
            .map_err(|error| error.to_string())?;
        self.with_tasks(|tasks| {
            for (id, mut record) in records {
                if let Some(current) = tasks.get(&id) {
                    record.control = current.control.clone();
                }
                tasks.insert(id, record);
            }
        })
        .map_err(|_| "task registry lock poisoned".to_string())
    }

    fn persist_current_task(
        &self,
        tasks: &HashMap<String, TaskRecord>,
        id: &str,
    ) -> Result<(), String> {
        let Some(persistence) = &self.persistence else {
            return Ok(());
        };
        let record = tasks
            .get(id)
            .ok_or_else(|| format!("task '{id}' not found"))?;
        persistence
            .write_current_task(record)
            .map_err(|error| error.to_string())
    }

    fn persist_typed_provider_outcomes(
        &self,
        outcomes: &HashMap<String, DurableTypedProviderOutcome>,
    ) -> Result<(), String> {
        let Some(persistence) = &self.persistence else {
            return Ok(());
        };
        persistence
            .write_typed_provider_outcomes(&self.session_id, outcomes)
            .map_err(|error| error.to_string())
    }

    fn with_tasks<R, F>(&self, f: F) -> Result<R, ()>
    where
        F: FnOnce(&mut HashMap<String, TaskRecord>) -> R,
    {
        let mut tasks = self.inner.lock().map_err(|_| ())?;
        Ok(f(&mut tasks))
    }
}

impl TaskPersistence {
    fn new(root: PathBuf, session_id: String) -> Self {
        Self { root, session_id }
    }

    fn write_current_task(&self, record: &TaskRecord) -> io::Result<()> {
        // The index maps a task id to its owning session and is written with
        // atomic replacement; readers never observe a partial file. The
        // session lock serializes the record file write. The global index
        // lock is taken only when the task is not yet indexed (creation), so
        // concurrent updates of already-indexed tasks never queue on it.
        let index = self.load_index()?;
        let session_id = index
            .get(&record.id)
            .cloned()
            .unwrap_or_else(|| self.session_id.clone());
        let already_indexed = index.contains_key(&record.id);
        let _session_lock = ExclusiveFileLock::acquire(&self.session_lock_path(&session_id))
            .map_err(io::Error::other)?;
        let (mut records, _) = self.read_session_records(&session_id)?;
        records.insert(record.id.clone(), record.clone());
        self.write_session_records_unlocked(&session_id, &records)?;
        if already_indexed {
            return Ok(());
        }
        let _index_lock =
            ExclusiveFileLock::acquire(&self.index_lock_path()).map_err(io::Error::other)?;
        let mut index = self.load_index()?;
        index.insert(record.id.clone(), session_id);
        self.write_index(&index)
    }

    fn mutate_current_task<R, F>(
        &self,
        id: &str,
        mutate: F,
    ) -> Result<(R, TaskRecord), TaskLeaseError>
    where
        F: FnOnce(&mut TaskRecord) -> Result<R, TaskLeaseError>,
    {
        // The index maps a task id to its owning session. It is written with
        // atomic replacement and read here without the index lock: the
        // session lock below is what serializes the actual record file write,
        // and index readers never observe a partially written file. Holding
        // the global index lock on every mutation would serialize all task
        // writes across every session.
        let index = self
            .load_index()
            .map_err(|error| TaskLeaseError::Persistence(error.to_string()))?;
        let session_id = index
            .get(id)
            .cloned()
            .unwrap_or_else(|| self.session_id.clone());
        let _session_lock = ExclusiveFileLock::acquire(&self.session_lock_path(&session_id))
            .map_err(|error| TaskLeaseError::Persistence(error.to_string()))?;
        let mut records = self
            .load_session_records_unlocked(&session_id)
            .map_err(|error| TaskLeaseError::Persistence(error.to_string()))?;
        let record = records.get_mut(id).ok_or(TaskLeaseError::NotFound)?;
        let result = mutate(record)?;
        let committed = record.clone();
        self.write_session_records_unlocked(&session_id, &records)
            .map_err(|error| TaskLeaseError::Persistence(error.to_string()))?;
        Ok((result, committed))
    }

    fn load_record_by_id(
        &self,
        id: &str,
        recover_interrupted: bool,
        requesting_session_id: &str,
    ) -> io::Result<Option<TaskRecord>> {
        let index = self.load_index()?;
        let Some(session_id) = index.get(id).cloned() else {
            return Ok(None);
        };
        let _session_lock = ExclusiveFileLock::acquire(&self.session_lock_path(&session_id))
            .map_err(io::Error::other)?;
        let mut records = self.load_session_records_unlocked(&session_id)?;
        let Some(record) = records.get_mut(id) else {
            return Ok(None);
        };
        if recover_interrupted
            && session_id != requesting_session_id
            && mark_interrupted_if_active(record)
        {
            self.write_session_records_unlocked(&session_id, &records)?;
        }
        Ok(records.remove(id))
    }

    fn load_session_records(&self, session_id: &str) -> io::Result<HashMap<String, TaskRecord>> {
        let _session_lock = ExclusiveFileLock::acquire(&self.session_lock_path(session_id))
            .map_err(io::Error::other)?;
        self.load_session_records_unlocked(session_id)
    }

    fn load_session_records_unlocked(
        &self,
        session_id: &str,
    ) -> io::Result<HashMap<String, TaskRecord>> {
        let (records, changed) = self.read_session_records(session_id)?;
        if changed {
            self.write_session_records_unlocked(session_id, &records)?;
        }
        Ok(records)
    }

    fn read_session_records(
        &self,
        session_id: &str,
    ) -> io::Result<(HashMap<String, TaskRecord>, bool)> {
        let path = self.session_tasks_path(session_id);
        if !path.exists() {
            return Ok((HashMap::new(), false));
        }
        let persisted: HashMap<String, PersistedTaskRecord> = read_json(&path)?;
        let mut changed = false;
        let records = persisted
            .into_iter()
            .map(|(id, record)| {
                let (record, record_changed) = TaskRecord::from_persisted(record);
                changed |= record_changed;
                (id, record)
            })
            .collect::<HashMap<_, _>>();
        Ok((records, changed))
    }

    fn write_session_records_unlocked(
        &self,
        session_id: &str,
        records: &HashMap<String, TaskRecord>,
    ) -> io::Result<()> {
        let persisted = records
            .iter()
            .map(|(id, record)| (id.clone(), PersistedTaskRecord::from(record)))
            .collect::<HashMap<_, _>>();
        write_json_pretty(&self.session_tasks_path(session_id), &persisted)
    }

    fn load_typed_provider_outcomes_unlocked(
        &self,
        session_id: &str,
    ) -> io::Result<HashMap<String, DurableTypedProviderOutcome>> {
        let path = self.session_typed_provider_outcomes_path(session_id);
        if !path.exists() {
            return Ok(HashMap::new());
        }
        let persisted: HashMap<String, PersistedTypedProviderOutcome> = read_json(&path)?;
        persisted
            .into_iter()
            .map(|(task_id, outcome)| {
                DurableTypedProviderOutcome::try_from(outcome).map(|outcome| (task_id, outcome))
            })
            .collect()
    }

    fn write_typed_provider_outcomes(
        &self,
        session_id: &str,
        outcomes: &HashMap<String, DurableTypedProviderOutcome>,
    ) -> io::Result<()> {
        let _session_lock = ExclusiveFileLock::acquire(&self.session_lock_path(session_id))
            .map_err(io::Error::other)?;
        self.write_typed_provider_outcomes_unlocked(session_id, outcomes)
    }

    fn write_typed_provider_outcomes_unlocked(
        &self,
        session_id: &str,
        outcomes: &HashMap<String, DurableTypedProviderOutcome>,
    ) -> io::Result<()> {
        #[cfg(test)]
        {
            let mut failures = TYPED_PROVIDER_OUTCOME_WRITE_FAILURES
                .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let remaining = failures.get_mut(session_id);
            if let Some(remaining) = remaining {
                if *remaining > 1 {
                    *remaining -= 1;
                } else {
                    failures.remove(session_id);
                }
                return Err(io::Error::other(
                    "injected typed provider outcome persistence failure",
                ));
            }
        }
        let persisted = outcomes
            .iter()
            .map(|(task_id, outcome)| {
                (
                    task_id.clone(),
                    PersistedTypedProviderOutcome::from(outcome),
                )
            })
            .collect::<HashMap<_, _>>();
        write_json_pretty(
            &self.session_typed_provider_outcomes_path(session_id),
            &persisted,
        )
    }

    fn load_index(&self) -> io::Result<HashMap<String, String>> {
        let path = self.index_path();
        if !path.exists() {
            return Ok(HashMap::new());
        }
        read_json(&path)
    }

    fn write_index(&self, index: &HashMap<String, String>) -> io::Result<()> {
        write_json_pretty(&self.index_path(), index)
    }

    fn session_tasks_path(&self, session_id: &str) -> PathBuf {
        self.root
            .join(safe_path_component(session_id))
            .join("tasks.json")
    }

    fn session_typed_provider_outcomes_path(&self, session_id: &str) -> PathBuf {
        self.root
            .join(safe_path_component(session_id))
            .join("typed-provider-outcomes.json")
    }

    fn session_lock_path(&self, session_id: &str) -> PathBuf {
        self.root
            .join(safe_path_component(session_id))
            .join("tasks.lock")
    }

    fn index_path(&self) -> PathBuf {
        self.root.join("task-index.json")
    }

    fn index_lock_path(&self) -> PathBuf {
        self.root.join("task-index.lock")
    }
}

impl RuntimeSubagentStatusLookup for TaskRegistry {
    fn subagent_status_record(&self, agent_id: &str) -> Option<RuntimeSubagentStatusRecord> {
        let record = self.get(agent_id)?;
        if record.task_type != TaskType::Subagent {
            return None;
        }
        Some(RuntimeSubagentStatusRecord {
            id: record.id,
            status: serde_json::to_value(record.status)
                .ok()
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_else(|| format!("{:?}", record.status)),
            description: record.description,
            agent_type: record.agent_type,
            created_at_ms: record.created_at_ms,
            started_at_ms: record.started_at_ms,
            completed_at_ms: record.completed_at_ms,
            output: record.result,
            error: record.error,
            usage: record.usage.map(|usage| RuntimeUsageTotals {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                cache_tokens: usage.cache_tokens,
                estimated_cost_usd: usage.estimated_cost_usd,
            }),
            subagent_current_activity: record.subagent_current_activity,
            subagent_turn: record.subagent_turn,
            last_activity_at_ms: record.last_activity_at_ms,
            continuation_id: record.continuation_id.map(|id| id.to_string()),
            continuation_attempt_id: record.continuation_attempt_id.map(|id| id.to_string()),
            continuation_checkpoint_id: record.continuation_checkpoint_id.map(|id| id.to_string()),
            continuation_resumable: record.continuation_resumable,
            continuation_indeterminate: record.continuation_indeterminate,
        })
    }
}

impl PersistedTaskRecord {
    fn into_task_record(self) -> (TaskRecord, bool) {
        let mut changed = false;
        let pending_provider_response = self
            .pending_provider_response
            .map(|value| serde_json::from_value::<PersistedProviderResponse>(value))
            .transpose();
        let mut record = TaskRecord {
            id: self.id,
            parent_task_id: self.parent_task_id,
            task_type: self.task_type,
            status: self.status,
            is_backgrounded: self.is_backgrounded,
            description: self.description,
            created_at_ms: self.created_at_ms,
            started_at_ms: self.started_at_ms,
            completed_at_ms: self.completed_at_ms,
            name: self.name,
            agent_type: self.agent_type,
            tool: self.tool,
            pending_tool_call: self.pending_tool_call,
            pending_tool_approval_response: self.pending_tool_approval_response,
            pending_provider_response: None,
            workflow_run_id: self.workflow_run_id,
            phase_count: self.phase_count,
            workflow_progress: self.workflow_progress,
            workflow_phases: self.workflow_phases,
            workflow_agents: self.workflow_agents,
            workflow_script_path: self.workflow_script_path,
            workflow_launch_input: self.workflow_launch_input,
            workflow_final_summary: self.workflow_final_summary,
            workflow_failure_count: self.workflow_failure_count,
            usage: self.usage,
            subagent_current_activity: self.subagent_current_activity,
            subagent_turn: self.subagent_turn,
            last_activity_at_ms: self.last_activity_at_ms,
            continuation_id: self.continuation_id,
            continuation_attempt_id: self.continuation_attempt_id,
            continuation_checkpoint_id: self.continuation_checkpoint_id,
            continuation_revision: self.continuation_revision,
            continuation_resumable: self.continuation_resumable,
            continuation_indeterminate: self.continuation_indeterminate,
            result: self.result,
            error: self.error,
            retry_count: self.retry_count,
            output_truncated: self.output_truncated,
            worker_pid: self.worker_pid,
            command: self.command,
            lease_owner: self.lease_owner,
            lease_epoch: self.lease_epoch,
            lease_expires_at_ms: self.lease_expires_at_ms,
            stop_requested: self.stop_requested,
            publication_revision: self.publication_revision,
            control: new_task_control(),
        };
        match pending_provider_response {
            Ok(Some(response)) => {
                record.pending_provider_response =
                    Some(response.into_runtime_response_with_migration_identity());
            }
            Ok(None) => {}
            Err(error) => {
                fail_invalid_pending_provider_response(&mut record, &error);
                changed = true;
            }
        }
        (record, changed)
    }
}

impl TaskRecord {
    fn from_persisted(record: PersistedTaskRecord) -> (Self, bool) {
        record.into_task_record()
    }

    fn continuation_projection(&self) -> Option<TaskContinuationProjection> {
        Some(TaskContinuationProjection {
            continuation_id: self.continuation_id.clone()?,
            attempt_id: self.continuation_attempt_id.clone()?,
            checkpoint_id: self.continuation_checkpoint_id.clone(),
            revision: self.continuation_revision?,
            resumable: self.continuation_resumable,
            indeterminate: self.continuation_indeterminate,
        })
    }

    fn set_continuation_projection(&mut self, projection: TaskContinuationProjection) {
        self.continuation_id = Some(projection.continuation_id);
        self.continuation_attempt_id = Some(projection.attempt_id);
        self.continuation_checkpoint_id = projection.checkpoint_id;
        self.continuation_revision = Some(projection.revision);
        self.continuation_resumable = projection.resumable;
        self.continuation_indeterminate = projection.indeterminate;
    }
}

impl From<&TaskRecord> for PersistedTaskRecord {
    fn from(record: &TaskRecord) -> Self {
        Self {
            id: record.id.clone(),
            parent_task_id: record.parent_task_id.clone(),
            task_type: record.task_type,
            status: record.status,
            is_backgrounded: record.is_backgrounded,
            description: record.description.clone(),
            created_at_ms: record.created_at_ms,
            started_at_ms: record.started_at_ms,
            completed_at_ms: record.completed_at_ms,
            name: record.name.clone(),
            agent_type: record.agent_type.clone(),
            tool: record.tool.clone(),
            pending_tool_call: record.pending_tool_call.clone(),
            pending_tool_approval_response: record.pending_tool_approval_response,
            pending_provider_response: record.pending_provider_response.as_ref().and_then(
                |response| serde_json::to_value(PersistedProviderResponse::from(response)).ok(),
            ),
            workflow_run_id: record.workflow_run_id.clone(),
            phase_count: record.phase_count,
            workflow_progress: record.workflow_progress,
            workflow_phases: record.workflow_phases.clone(),
            workflow_agents: record.workflow_agents.clone(),
            workflow_script_path: record.workflow_script_path.clone(),
            workflow_launch_input: record.workflow_launch_input.clone(),
            workflow_final_summary: record.workflow_final_summary.clone(),
            workflow_failure_count: record.workflow_failure_count,
            usage: record.usage,
            subagent_current_activity: record.subagent_current_activity.clone(),
            subagent_turn: record.subagent_turn,
            last_activity_at_ms: record.last_activity_at_ms,
            continuation_id: record.continuation_id.clone(),
            continuation_attempt_id: record.continuation_attempt_id.clone(),
            continuation_checkpoint_id: record.continuation_checkpoint_id.clone(),
            continuation_revision: record.continuation_revision,
            continuation_resumable: record.continuation_resumable,
            continuation_indeterminate: record.continuation_indeterminate,
            result: record.result.clone(),
            error: record.error.as_deref().map(redact_sensitive_text),
            retry_count: record.retry_count,
            output_truncated: record.output_truncated,
            worker_pid: record.worker_pid,
            command: record.command.clone(),
            lease_owner: record.lease_owner.clone(),
            lease_epoch: record.lease_epoch,
            lease_expires_at_ms: record.lease_expires_at_ms,
            stop_requested: record.stop_requested,
            publication_revision: record.publication_revision,
        }
    }
}

impl TryFrom<PersistedTypedProviderOutcome> for DurableTypedProviderOutcome {
    type Error = io::Error;

    fn try_from(outcome: PersistedTypedProviderOutcome) -> Result<Self, Self::Error> {
        let response = outcome
            .response
            .map(|value| serde_json::from_value::<PersistedProviderResponse>(value))
            .transpose()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
            .map(PersistedProviderResponse::into_runtime_response_with_migration_identity);
        Ok(Self {
            status: outcome.status,
            response,
            error: outcome.error,
            usage: outcome.usage,
            operation_terminal: outcome.operation_terminal,
            completed_at_ms: outcome.completed_at_ms,
        })
    }
}

impl From<&DurableTypedProviderOutcome> for PersistedTypedProviderOutcome {
    fn from(outcome: &DurableTypedProviderOutcome) -> Self {
        Self {
            status: outcome.status,
            response: outcome.response.as_ref().and_then(|response| {
                serde_json::to_value(PersistedProviderResponse::from(response)).ok()
            }),
            error: outcome.error.as_deref().map(redact_sensitive_text),
            usage: outcome.usage,
            operation_terminal: outcome.operation_terminal.clone(),
            completed_at_ms: outcome.completed_at_ms,
        }
    }
}

impl PersistedProviderResponse {
    fn into_runtime_response_with_migration_identity(self) -> RuntimeModelResponse {
        let response = ProviderResponse {
            steps: self
                .steps
                .into_iter()
                .map(PersistedProviderStep::into_provider_step)
                .collect(),
            assistant_content: self.assistant_content,
            assistant_reasoning: self.assistant_reasoning,
            tool_calls: self.tool_calls,
            usage: self.usage,
        };
        RuntimeModelResponse::from_parts(
            response,
            self.identity
                .unwrap_or_else(|| ModelResponseIdentity::new(TurnId::new())),
        )
    }
}

impl From<&RuntimeModelResponse> for PersistedProviderResponse {
    fn from(response: &RuntimeModelResponse) -> Self {
        Self {
            steps: response
                .response
                .steps
                .iter()
                .filter_map(PersistedProviderStep::from_provider_step)
                .collect(),
            assistant_content: response.response.assistant_content.clone(),
            assistant_reasoning: response.response.assistant_reasoning.clone(),
            tool_calls: response.response.tool_calls.clone(),
            usage: response.response.usage,
            identity: Some(response.identity.clone()),
        }
    }
}

impl PersistedProviderStep {
    fn from_provider_step(step: &ProviderStep) -> Option<Self> {
        match step {
            ProviderStep::ReasoningDelta(delta) => Some(Self::ReasoningDelta(delta.clone())),
            ProviderStep::MessageDelta(delta) => Some(Self::MessageDelta(delta.clone())),
            ProviderStep::ToolCallProgress(progress) => {
                Some(Self::ToolCallProgress(progress.clone()))
            }
            ProviderStep::ToolCall(request) => Some(Self::ToolCall(request.clone())),
            ProviderStep::ReplayState(_) => None,
            ProviderStep::Error(error) => Some(Self::Error(PersistedProviderError::Classified {
                kind: error.kind,
                message: error.message.clone(),
            })),
        }
    }

    fn into_provider_step(self) -> ProviderStep {
        match self {
            Self::ReasoningDelta(delta) => ProviderStep::ReasoningDelta(delta),
            Self::MessageDelta(delta) => ProviderStep::MessageDelta(delta),
            Self::ToolCallProgress(progress) => ProviderStep::ToolCallProgress(progress),
            Self::ToolCall(request) => ProviderStep::ToolCall(request),
            Self::Error(PersistedProviderError::Legacy(message)) => {
                ProviderStep::Error(ProviderError::other(message))
            }
            Self::Error(PersistedProviderError::Classified { kind, message }) => {
                ProviderStep::Error(ProviderError::new(kind, message))
            }
        }
    }
}

fn fail_invalid_pending_provider_response(record: &mut TaskRecord, error: &serde_json::Error) {
    record.status = TaskStatus::Failed;
    if record.started_at_ms.is_none() {
        record.started_at_ms = Some(now_ms());
    }
    record.completed_at_ms = Some(now_ms());
    record.result = None;
    record.error = Some(format!(
        "invalid pending provider response; background continuation failed closed: {error}"
    ));
    record.tool = None;
    record.pending_tool_call = None;
    record.pending_tool_approval_response = None;
    record.pending_provider_response = None;
    record.worker_pid = None;
    record.control.cancel.cancel();
    record.control.pause.store(false, Ordering::Release);
}

fn new_task_control() -> TaskControl {
    TaskControl {
        cancel: CancelToken::new(),
        pause: Arc::new(AtomicBool::new(false)),
        worker: Arc::new(Mutex::new(None)),
    }
}

fn spawn_worker_reaper(
    registry: TaskRegistry,
    task_id: String,
    worker_pid: u32,
    adoption_revision: u64,
    worker: Arc<Mutex<Option<OwnedWorker>>>,
) {
    thread::spawn(move || {
        loop {
            let finished = {
                let mut slot = worker
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                let Some(owned_worker) = slot.as_mut() else {
                    return;
                };
                match owned_worker.child.try_wait() {
                    Ok(Some(_)) => true,
                    Ok(None) => false,
                    Err(_) => {
                        terminate_worker(owned_worker);
                        let _ = owned_worker.child.wait();
                        true
                    }
                }
            };
            if finished {
                let mut slot = worker
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                slot.take();
                drop(slot);
                let refreshed = registry
                    .refresh_task_from_persistence(&task_id)
                    .ok()
                    .flatten();
                if refreshed
                    .as_ref()
                    .is_some_and(|record| is_terminal(record.status))
                {
                    return;
                }
                if let Ok(Some(fence)) =
                    registry.worker_exit_fence(&task_id, worker_pid, adoption_revision)
                {
                    let _ = registry.fail_worker_exit_if_current(&task_id, &fence);
                }
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
    });
}

fn task_summary(record: &TaskRecord) -> BackgroundTaskSummary {
    BackgroundTaskSummary {
        id: record.id.clone(),
        parent_task_id: record.parent_task_id.clone(),
        task_type: record.task_type,
        status: record.status,
        is_backgrounded: record.is_backgrounded,
        description: record.description.clone(),
        created_at_ms: record.created_at_ms,
        started_at_ms: record.started_at_ms,
        completed_at_ms: record.completed_at_ms,
        command: record.command.clone(),
        agent_type: record.agent_type.clone(),
        server: None,
        tool: record.tool.clone(),
        pending_tool_call: record.pending_tool_call.clone(),
        name: record.name.clone(),
        workflow_run_id: record.workflow_run_id.clone(),
        phase_count: record.phase_count,
        workflow_progress: record.workflow_progress,
        workflow_phases: record.workflow_phases.clone(),
        workflow_agents: record.workflow_agents.clone(),
        workflow_script_path: record.workflow_script_path.clone(),
        workflow_launch_input: record.workflow_launch_input.clone(),
        workflow_final_summary: record.workflow_final_summary.clone(),
        workflow_failure_count: record.workflow_failure_count,
        usage: record.usage,
        subagent_current_activity: record.subagent_current_activity.clone(),
        subagent_turn: record.subagent_turn,
        last_activity_at_ms: record.last_activity_at_ms,
        continuation: record
            .continuation_projection()
            .map(|projection| TaskContinuationSummary {
                continuation_id: projection.continuation_id.to_string(),
                attempt_id: projection.attempt_id.to_string(),
                checkpoint_id: projection.checkpoint_id.map(|id| id.to_string()),
                revision: projection.revision.get(),
                resumable: projection.resumable,
                indeterminate: projection.indeterminate,
            }),
        result: record.result.clone(),
        error: record.error.clone(),
        retry_count: record.retry_count,
        output_truncated: record.output_truncated,
        publication_revision: Some(record.publication_revision),
    }
}

fn terminate_worker(worker: &mut OwnedWorker) {
    let _ = worker.process_job.terminate(137);
    orca_tools::process::kill_child_tree(&mut worker.child);
}

#[cfg(unix)]
const SUBAGENT_WORKER_PROCESS_PREFIX: &str = "orca-subagent-worker-";

#[cfg(unix)]
pub(crate) fn subagent_worker_process_name(agent_id: &str) -> String {
    format!("{SUBAGENT_WORKER_PROCESS_PREFIX}{agent_id}")
}

#[cfg(unix)]
fn verify_recovered_worker(pid: u32, agent_id: &str) -> Result<RecoveredWorkerState, String> {
    unsafe extern "C" {
        fn getpgid(pid: i32) -> i32;
    }

    let pid = i32::try_from(pid).map_err(|_| "worker PID exceeds Unix pid_t".to_string())?;
    let pgid = unsafe { getpgid(pid) };
    if pgid < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(3) {
            return Ok(RecoveredWorkerState::Missing);
        }
        return Err(format!("failed to inspect async subagent worker: {error}"));
    }
    if pgid != pid {
        return Ok(RecoveredWorkerState::Replaced);
    }

    let ps = [Path::new("/bin/ps"), Path::new("/usr/bin/ps")]
        .into_iter()
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| "cannot verify async subagent worker without ps".to_string())?;
    let pid_text = pid.to_string();
    let output = Command::new(ps)
        .args(["-ww", "-p", pid_text.as_str(), "-o", "command="])
        .output()
        .map_err(|error| format!("failed to inspect async subagent worker: {error}"))?;
    if !output.status.success() {
        let pgid = unsafe { getpgid(pid) };
        if pgid < 0 && io::Error::last_os_error().raw_os_error() == Some(3) {
            return Ok(RecoveredWorkerState::Missing);
        }
        return Err("failed to inspect async subagent worker command".to_string());
    }
    if !subagent_worker_command_matches(&String::from_utf8_lossy(&output.stdout), agent_id) {
        return Ok(RecoveredWorkerState::Replaced);
    }
    Ok(RecoveredWorkerState::Matches)
}

#[cfg(unix)]
fn subagent_worker_command_matches(command_line: &str, agent_id: &str) -> bool {
    let arguments = command_line.split_whitespace().collect::<Vec<_>>();
    let expected = subagent_worker_process_name(agent_id);
    if arguments.first().copied() == Some(expected.as_str()) {
        return true;
    }
    arguments.get(1).copied() == Some("subagent-worker")
        && arguments
            .windows(2)
            .any(|pair| pair[0] == "--agent-id" && pair[1] == agent_id)
}

#[cfg(windows)]
fn verify_recovered_worker(pid: u32, agent_id: &str) -> Result<RecoveredWorkerState, String> {
    match ProcessJob::open_named(&async_worker_job_name(agent_id)) {
        Ok(job) => job
            .contains_process(pid)
            .map(|matches| {
                if matches {
                    RecoveredWorkerState::Matches
                } else {
                    RecoveredWorkerState::Replaced
                }
            })
            .map_err(|error| format!("failed to inspect async subagent worker job: {error}")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(RecoveredWorkerState::Missing),
        Err(error) => Err(format!("failed to open async subagent worker job: {error}")),
    }
}

#[cfg(not(any(unix, windows)))]
fn verify_recovered_worker(_pid: u32, _agent_id: &str) -> Result<RecoveredWorkerState, String> {
    Err("cannot safely verify a recovered async subagent worker on this platform".to_string())
}

#[cfg(unix)]
fn terminate_recovered_worker(pid: u32, agent_id: &str) -> Result<(), String> {
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }

    if !matches!(
        verify_recovered_worker(pid, agent_id)?,
        RecoveredWorkerState::Matches
    ) {
        return Ok(());
    }
    let pid = i32::try_from(pid).map_err(|_| "worker PID exceeds Unix pid_t".to_string())?;
    let process_group = -pid;
    if unsafe { kill(process_group, 15) } < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(3) {
            return Ok(());
        }
        return Err(format!(
            "failed to signal async subagent worker process group with SIGTERM: {error}"
        ));
    }
    // A shell can defer TERM handling while it waits for a child in the same
    // process group. Give both handlers time to settle before escalating.
    thread::sleep(Duration::from_millis(250));
    if !process_group_has_live_members(pid)? {
        return Ok(());
    }
    if unsafe { kill(process_group, 9) } < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(3) || !process_group_has_live_members(pid)? {
            return Ok(());
        }
        return Err(format!(
            "failed to signal async subagent worker process group with SIGKILL: {error}"
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn process_group_has_live_members(pgid: i32) -> Result<bool, String> {
    let ps = [Path::new("/bin/ps"), Path::new("/usr/bin/ps")]
        .into_iter()
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| {
            "cannot inspect async subagent worker process group without ps".to_string()
        })?;
    let output = Command::new(ps)
        .args(["-ax", "-o", "pgid=", "-o", "stat="])
        .output()
        .map_err(|error| format!("failed to inspect async subagent worker group: {error}"))?;
    if !output.status.success() {
        return Err("failed to inspect async subagent worker process group".to_string());
    }
    Ok(String::from_utf8_lossy(&output.stdout).lines().any(|line| {
        let mut fields = line.split_whitespace();
        fields
            .next()
            .and_then(|value| value.parse::<i32>().ok())
            .is_some_and(|candidate| candidate == pgid)
            && fields.next().is_some_and(|state| !state.starts_with('Z'))
    }))
}

#[cfg(windows)]
fn terminate_recovered_worker(pid: u32, agent_id: &str) -> Result<(), String> {
    let job = match ProcessJob::open_named(&async_worker_job_name(agent_id)) {
        Ok(job) => job,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!("failed to open async subagent worker job: {error}"));
        }
    };
    if !job
        .contains_process(pid)
        .map_err(|error| format!("failed to inspect async subagent worker job: {error}"))?
    {
        return Ok(());
    }
    job.terminate(137)
        .map_err(|error| format!("failed to terminate async subagent worker job: {error}"))
}

#[cfg(not(any(unix, windows)))]
fn terminate_recovered_worker(_pid: u32, _agent_id: &str) -> Result<(), String> {
    Err("cannot safely stop a recovered async subagent worker on this platform".to_string())
}

fn new_task_id() -> String {
    format!("task-{}", uuid::Uuid::new_v4())
}

fn new_task_lease_owner_id(process_id: u32) -> String {
    format!("pid:{process_id}:{}", uuid::Uuid::new_v4())
}

fn task_lease_owner_pid(owner_id: &str) -> Option<u32> {
    let (prefix, suffix) = owner_id.split_once(':')?;
    if prefix != "pid" {
        return None;
    }
    suffix.split_once(':')?.0.parse().ok()
}

fn task_depth_below(
    tasks: &HashMap<String, TaskRecord>,
    task_id: &str,
    root_id: &str,
) -> Option<usize> {
    let mut current = task_id;
    let mut depth = 0usize;
    let mut visited = HashSet::new();
    loop {
        if current == root_id {
            return Some(depth);
        }
        if !visited.insert(current) {
            return None;
        }
        current = tasks.get(current)?.parent_task_id.as_deref()?;
        depth = depth.checked_add(1)?;
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn validate_task_lease(
    record: &TaskRecord,
    lease: &TaskLease,
    owner_id: &str,
) -> Result<(), TaskLeaseError> {
    if lease.task_id != record.id
        || lease.owner_id != owner_id
        || record.lease_owner.as_deref() != Some(owner_id)
        || record.lease_epoch != lease.epoch
        || record
            .lease_expires_at_ms
            .is_none_or(|expires_at_ms| expires_at_ms <= now_ms())
    {
        return Err(TaskLeaseError::Fenced);
    }
    Ok(())
}

fn is_terminal(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Stopped
            | TaskStatus::Completed
            | TaskStatus::Failed
            | TaskStatus::ApprovalRequired
            | TaskStatus::Cancelled
    )
}

fn terminal_main_session_reconciliation_eligible(record: &TaskRecord) -> bool {
    record.task_type == TaskType::MainSession
        && matches!(
            record.status,
            TaskStatus::Completed | TaskStatus::Stopped | TaskStatus::Cancelled
        )
        && SurfaceTaskId::try_new(record.id.clone()).is_ok()
        && record.completed_at_ms.is_some()
        && record.worker_pid.is_none()
        && record.lease_owner.is_none()
        && record.pending_tool_call.is_none()
        && record.pending_provider_response.is_none()
        && record.pending_tool_approval_response.is_none()
}

fn active_main_session_adoption_eligible(record: &TaskRecord) -> bool {
    record.task_type == TaskType::MainSession
        && record.status == TaskStatus::Running
        && SurfaceTaskId::try_new(record.id.clone()).is_ok()
        && record.started_at_ms.is_some()
        && record.completed_at_ms.is_none()
        && record.worker_pid.is_none()
        && record.lease_owner.is_none()
        && record.lease_expires_at_ms.is_none()
        && !record.stop_requested
        && record.tool.is_none()
        && record.pending_tool_call.is_none()
        && record.pending_provider_response.is_none()
        && record.pending_tool_approval_response.is_none()
        && record.result.is_none()
        && record.error.is_none()
}

fn pending_tool_call_from_provider_response(
    response: &ProviderResponse,
) -> Option<PendingToolCallSummary> {
    response
        .steps
        .iter()
        .find_map(|step| match step {
            ProviderStep::ToolCall(request) => Some(PendingToolCallSummary {
                id: request.id.clone(),
                name: request.name.as_str().to_string(),
                action: request.action,
                target: request.target.clone(),
                arguments: request
                    .raw_arguments
                    .clone()
                    .unwrap_or_else(|| "{}".to_string()),
            }),
            _ => None,
        })
        .or_else(|| {
            response
                .tool_calls
                .first()
                .map(|tool_call| PendingToolCallSummary {
                    id: tool_call.id.clone(),
                    name: tool_call.function_name.clone(),
                    action: orca_core::approval_types::ActionKind::Read,
                    target: None,
                    arguments: tool_call.arguments.clone(),
                })
        })
}

fn mark_interrupted_if_active(record: &mut TaskRecord) -> bool {
    if is_terminal(record.status) {
        return false;
    }
    if record.task_type == TaskType::Subagent && record.worker_pid.is_some() {
        return false;
    }
    record.status = TaskStatus::Failed;
    if record.started_at_ms.is_none() {
        record.started_at_ms = Some(now_ms());
    }
    record.completed_at_ms = Some(now_ms());
    record.result = None;
    record.error = Some(format!(
        "{} interrupted before completion; async task execution is process-local",
        task_type_label(record.task_type)
    ));
    record.control.cancel.cancel();
    record.control.pause.store(false, Ordering::Release);
    true
}

fn task_type_label(task_type: TaskType) -> &'static str {
    match task_type {
        TaskType::MainSession => "main session",
        TaskType::Workflow => "workflow",
        TaskType::Subagent => "subagent",
        TaskType::Shell => "shell",
        TaskType::Monitor => "monitor",
    }
}

pub(crate) fn safe_path_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn task_state_error(action: &str, status: TaskStatus) -> String {
    format!("cannot {action} task in {status:?} state")
}

fn write_json_pretty<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    atomic_write(path, content.as_bytes(), AtomicWritePolicy::NoFollow).map_err(io::Error::other)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> io::Result<T> {
    let content = fs::read_to_string(path)?;
    serde_json::from_str(&content)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn task_sessions_root() -> Option<PathBuf> {
    #[cfg(test)]
    let test_home = crate::history::read_test_orca_home();
    #[cfg(not(test))]
    let test_home = std::env::var_os("ORCA_HOME");
    test_home
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".orca")))
        .map(|home| home.join("task-sessions"))
}

fn legacy_project_task_sessions_root(cwd: &Path) -> PathBuf {
    cwd.join(".orca").join("task-sessions")
}

fn migrate_legacy_task_sessions(legacy_root: &Path, target_root: &Path) -> io::Result<()> {
    if legacy_root == target_root || !legacy_root.exists() {
        return Ok(());
    }

    let legacy = TaskPersistence::new(legacy_root.to_path_buf(), String::new());
    let target = TaskPersistence::new(target_root.to_path_buf(), String::new());
    let legacy_index = legacy.load_index()?;
    if legacy_index.is_empty() {
        return Ok(());
    }

    let _index_lock =
        ExclusiveFileLock::acquire(&target.index_lock_path()).map_err(io::Error::other)?;
    let mut target_index = target.load_index()?;
    let mut changed_index = false;
    let session_ids = legacy_index.values().cloned().collect::<HashSet<_>>();
    for session_id in session_ids {
        let legacy_records = legacy.load_session_records(&session_id)?;
        if legacy_records.is_empty() {
            continue;
        }

        let _session_lock = ExclusiveFileLock::acquire(&target.session_lock_path(&session_id))
            .map_err(io::Error::other)?;
        let mut target_records = target.load_session_records_unlocked(&session_id)?;
        let mut changed_session = false;
        for (id, record) in legacy_records {
            if legacy_index.get(&id) != Some(&session_id) || target_index.contains_key(&id) {
                continue;
            }
            target_records.insert(id.clone(), record);
            target_index.insert(id, session_id.clone());
            changed_session = true;
            changed_index = true;
        }

        if changed_session {
            target.write_session_records_unlocked(&session_id, &target_records)?;
        }
    }

    if changed_index {
        target.write_index(&target_index)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subagent_event_relay::RelayTaskType;

    fn test_surface_commit_id(seed: u8) -> crate::surface::SurfaceCommitId {
        let mut bytes = [seed; 16];
        bytes[6] = 0x70 | (seed & 0x0f);
        bytes[8] = 0x80 | (seed & 0x3f);
        crate::surface::SurfaceCommitId::try_from_bytes(bytes).unwrap()
    }

    #[test]
    fn persistent_terminal_reconciliation_receipt_filters_and_sorts_records() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tasks");
        let session_id = "terminal-reconciliation".to_string();
        let registry = TaskRegistry::new_persistent_attached(session_id.clone(), root).unwrap();
        let template_id = registry.create_main_session("template".to_string()).id;
        let template = registry.get(&template_id).expect("template record");

        let insert = |id: &str,
                      task_type: TaskType,
                      status: TaskStatus,
                      publication_revision: u64,
                      unsafe_state: bool| {
            let mut record = template.clone();
            record.id = id.to_string();
            record.task_type = task_type;
            record.status = status;
            record.description = format!("history {id}");
            record.started_at_ms = Some(20);
            record.completed_at_ms = is_terminal(status).then_some(30);
            record.result = is_terminal(status).then(|| format!("result {id}"));
            record.publication_revision = publication_revision;
            record.worker_pid = unsafe_state.then_some(42);
            record.lease_owner = unsafe_state.then(|| "legacy-owner".to_string());
            record.pending_tool_approval_response = unsafe_state.then_some(true);
            registry
                .insert_task(id.to_string(), record)
                .expect("persist reconciliation fixture");
        };

        insert(
            "legacy-z-completed",
            TaskType::MainSession,
            TaskStatus::Completed,
            7,
            false,
        );
        insert(
            "legacy-a-stopped",
            TaskType::MainSession,
            TaskStatus::Stopped,
            2,
            false,
        );
        insert(
            "legacy-m-cancelled",
            TaskType::MainSession,
            TaskStatus::Cancelled,
            5,
            false,
        );
        for (id, status) in [
            ("legacy-queued", TaskStatus::Queued),
            ("legacy-running", TaskStatus::Running),
            ("legacy-paused", TaskStatus::Paused),
            ("legacy-stopping", TaskStatus::Stopping),
            ("legacy-approval", TaskStatus::ApprovalRequired),
            ("legacy-failed", TaskStatus::Failed),
        ] {
            insert(id, TaskType::MainSession, status, 3, false);
        }
        insert(
            "legacy-workflow",
            TaskType::Workflow,
            TaskStatus::Completed,
            4,
            false,
        );
        insert(
            "legacy-unsafe-terminal",
            TaskType::MainSession,
            TaskStatus::Completed,
            8,
            true,
        );
        insert("", TaskType::MainSession, TaskStatus::Completed, 9, false);

        let mut missing_completion = template.clone();
        missing_completion.id = "legacy-missing-completion".to_string();
        missing_completion.status = TaskStatus::Completed;
        missing_completion.completed_at_ms = None;
        missing_completion.publication_revision = 10;
        registry
            .insert_task(missing_completion.id.clone(), missing_completion)
            .expect("persist missing-completion fixture");

        let mut worker_owned = template.clone();
        worker_owned.id = "legacy-worker-owned".to_string();
        worker_owned.status = TaskStatus::Completed;
        worker_owned.completed_at_ms = Some(30);
        worker_owned.worker_pid = Some(42);
        worker_owned.publication_revision = 11;
        registry
            .insert_task(worker_owned.id.clone(), worker_owned)
            .expect("persist worker-owned fixture");

        let mut leased = template.clone();
        leased.id = "legacy-leased".to_string();
        leased.status = TaskStatus::Completed;
        leased.completed_at_ms = Some(30);
        leased.lease_owner = Some("legacy-owner".to_string());
        leased.publication_revision = 12;
        registry
            .insert_task(leased.id.clone(), leased)
            .expect("persist leased fixture");

        let mut pending_tool = template.clone();
        pending_tool.id = "legacy-pending-tool".to_string();
        pending_tool.status = TaskStatus::Completed;
        pending_tool.completed_at_ms = Some(30);
        pending_tool.pending_tool_call = Some(PendingToolCallSummary {
            id: "legacy-tool-call".to_string(),
            name: "task_list".to_string(),
            action: orca_core::approval_types::ActionKind::Read,
            target: None,
            arguments: "{}".to_string(),
        });
        pending_tool.publication_revision = 13;
        registry
            .insert_task(pending_tool.id.clone(), pending_tool)
            .expect("persist pending-tool fixture");

        let mut pending_provider = template.clone();
        pending_provider.id = "legacy-pending-provider".to_string();
        pending_provider.status = TaskStatus::Completed;
        pending_provider.completed_at_ms = Some(30);
        pending_provider.pending_provider_response = Some(runtime_response(ProviderResponse {
            steps: Vec::new(),
            assistant_content: Some("legacy pending provider response".to_string()),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        }));
        pending_provider.publication_revision = 14;
        registry
            .insert_task(pending_provider.id.clone(), pending_provider)
            .expect("persist pending-provider fixture");

        let mut pending_approval = template.clone();
        pending_approval.id = "legacy-pending-approval".to_string();
        pending_approval.status = TaskStatus::Completed;
        pending_approval.completed_at_ms = Some(30);
        pending_approval.pending_tool_approval_response = Some(true);
        pending_approval.publication_revision = 15;
        registry
            .insert_task(pending_approval.id.clone(), pending_approval)
            .expect("persist pending-approval fixture");

        let first = registry
            .with_terminal_main_session_reconciliation(|receipt| {
                (
                    receipt.session_id().to_string(),
                    receipt.publication_horizon(),
                    receipt.digest(),
                    receipt
                        .records()
                        .iter()
                        .map(|record| {
                            (
                                record.id().to_string(),
                                record.status(),
                                record.publication_revision(),
                            )
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .expect("issue terminal reconciliation receipt")
            .expect("persistent registries issue receipts");
        let second_digest = registry
            .with_terminal_main_session_reconciliation(|receipt| receipt.digest())
            .expect("reissue terminal reconciliation receipt")
            .expect("persistent registries issue receipts");

        assert_eq!(first.0, session_id);
        assert_eq!(first.1, 8);
        assert_eq!(first.2, second_digest);
        assert_eq!(
            first.3,
            vec![
                ("legacy-a-stopped".to_string(), TaskStatus::Stopped, 2,),
                ("legacy-m-cancelled".to_string(), TaskStatus::Cancelled, 5,),
                ("legacy-z-completed".to_string(), TaskStatus::Completed, 7,),
            ]
        );
    }

    #[test]
    fn persistent_active_main_session_adoption_receipt_filters_and_sorts_records() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tasks");
        let session_id = "active-adoption".to_string();
        let registry = TaskRegistry::new_persistent_attached(session_id.clone(), root).unwrap();
        let template_id = registry.create_main_session("template".to_string()).id;
        let template = registry.get(&template_id).expect("template record");

        let insert = |id: &str, status: TaskStatus, publication_revision: u64| {
            let mut record = template.clone();
            record.id = id.to_string();
            record.status = status;
            record.description = format!("active {id}");
            record.started_at_ms = Some(20);
            record.completed_at_ms = None;
            record.publication_revision = publication_revision;
            registry
                .insert_task(id.to_string(), record)
                .expect("persist active-adoption fixture");
        };

        insert("legacy-z-running", TaskStatus::Running, 7);
        insert("legacy-a-running", TaskStatus::Running, 2);
        insert("legacy-provider-outcome", TaskStatus::Running, 6);
        for (id, status) in [
            ("legacy-queued", TaskStatus::Queued),
            ("legacy-paused", TaskStatus::Paused),
            ("legacy-stopping", TaskStatus::Stopping),
            ("legacy-approval", TaskStatus::ApprovalRequired),
            ("legacy-failed", TaskStatus::Failed),
        ] {
            insert(id, status, 3);
        }
        for (id, task_type) in [
            ("legacy-workflow", TaskType::Workflow),
            ("legacy-subagent", TaskType::Subagent),
            ("legacy-shell", TaskType::Shell),
            ("legacy-monitor", TaskType::Monitor),
        ] {
            let mut record = template.clone();
            record.id = id.to_string();
            record.task_type = task_type;
            record.status = TaskStatus::Running;
            record.started_at_ms = Some(20);
            record.publication_revision = 4;
            registry.insert_task(record.id.clone(), record).unwrap();
        }

        let mut no_started_at = template.clone();
        no_started_at.id = "legacy-no-start".to_string();
        no_started_at.status = TaskStatus::Running;
        no_started_at.publication_revision = 8;
        registry
            .insert_task(no_started_at.id.clone(), no_started_at)
            .unwrap();

        let mut residual_result = template.clone();
        residual_result.id = "legacy-result".to_string();
        residual_result.status = TaskStatus::Running;
        residual_result.started_at_ms = Some(20);
        residual_result.result = Some("stale result".to_string());
        residual_result.publication_revision = 9;
        registry
            .insert_task(residual_result.id.clone(), residual_result)
            .unwrap();

        let mut residual_error = template.clone();
        residual_error.id = "legacy-error".to_string();
        residual_error.status = TaskStatus::Running;
        residual_error.started_at_ms = Some(20);
        residual_error.error = Some("stale error".to_string());
        residual_error.publication_revision = 9;
        registry
            .insert_task(residual_error.id.clone(), residual_error)
            .unwrap();

        let mut residual_tool = template.clone();
        residual_tool.id = "legacy-tool".to_string();
        residual_tool.status = TaskStatus::Running;
        residual_tool.started_at_ms = Some(20);
        residual_tool.tool = Some("task_list".to_string());
        residual_tool.publication_revision = 9;
        registry
            .insert_task(residual_tool.id.clone(), residual_tool)
            .unwrap();

        let mut completed_at = template.clone();
        completed_at.id = "legacy-completed-at".to_string();
        completed_at.status = TaskStatus::Running;
        completed_at.started_at_ms = Some(20);
        completed_at.completed_at_ms = Some(30);
        completed_at.publication_revision = 9;
        registry
            .insert_task(completed_at.id.clone(), completed_at)
            .unwrap();

        let mut worker_owned = template.clone();
        worker_owned.id = "legacy-worker".to_string();
        worker_owned.status = TaskStatus::Running;
        worker_owned.started_at_ms = Some(20);
        worker_owned.worker_pid = Some(42);
        worker_owned.publication_revision = 10;
        registry
            .insert_task(worker_owned.id.clone(), worker_owned)
            .unwrap();

        let mut leased = template.clone();
        leased.id = "legacy-lease".to_string();
        leased.status = TaskStatus::Running;
        leased.started_at_ms = Some(20);
        leased.lease_owner = Some("legacy-owner".to_string());
        leased.lease_expires_at_ms = Some(now_ms().saturating_add(10_000));
        leased.publication_revision = 11;
        registry.insert_task(leased.id.clone(), leased).unwrap();

        let mut stop_requested = template.clone();
        stop_requested.id = "legacy-stop-requested".to_string();
        stop_requested.status = TaskStatus::Running;
        stop_requested.started_at_ms = Some(20);
        stop_requested.stop_requested = true;
        stop_requested.publication_revision = 12;
        registry
            .insert_task(stop_requested.id.clone(), stop_requested)
            .unwrap();

        let mut pending_tool = template.clone();
        pending_tool.id = "legacy-pending-tool".to_string();
        pending_tool.status = TaskStatus::Running;
        pending_tool.started_at_ms = Some(20);
        pending_tool.pending_tool_call = Some(PendingToolCallSummary {
            id: "tool-call".to_string(),
            name: "task_list".to_string(),
            action: orca_core::approval_types::ActionKind::Read,
            target: None,
            arguments: "{}".to_string(),
        });
        pending_tool.publication_revision = 13;
        registry
            .insert_task(pending_tool.id.clone(), pending_tool)
            .unwrap();

        let mut pending_approval = template.clone();
        pending_approval.id = "legacy-pending-approval".to_string();
        pending_approval.status = TaskStatus::Running;
        pending_approval.started_at_ms = Some(20);
        pending_approval.pending_tool_approval_response = Some(true);
        pending_approval.publication_revision = 13;
        registry
            .insert_task(pending_approval.id.clone(), pending_approval)
            .unwrap();

        let mut pending_provider = template.clone();
        pending_provider.id = "legacy-pending-provider".to_string();
        pending_provider.status = TaskStatus::Running;
        pending_provider.started_at_ms = Some(20);
        pending_provider.pending_provider_response = Some(runtime_response(ProviderResponse {
            steps: Vec::new(),
            assistant_content: Some("pending provider".to_string()),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
        }));
        pending_provider.publication_revision = 13;
        registry
            .insert_task(pending_provider.id.clone(), pending_provider)
            .unwrap();

        let mut malformed_id = template.clone();
        malformed_id.id = String::new();
        malformed_id.status = TaskStatus::Running;
        malformed_id.started_at_ms = Some(20);
        malformed_id.publication_revision = 14;
        registry
            .insert_task(malformed_id.id.clone(), malformed_id)
            .unwrap();

        registry
            .record_typed_provider_outcome(
                "legacy-provider-outcome",
                DurableTypedProviderOutcome {
                    status: TaskStatus::Stopped,
                    response: None,
                    error: Some("terminal provider outcome".to_string()),
                    usage: None,
                    operation_terminal: None,
                    completed_at_ms: 40,
                },
            )
            .unwrap();

        let observed = registry
            .with_active_main_session_adoption(|receipt| {
                assert!(receipt.is_valid());
                (
                    receipt.session_id().to_string(),
                    receipt.publication_horizon(),
                    receipt.digest(),
                    receipt
                        .records()
                        .iter()
                        .map(|record| {
                            (
                                record.id().to_string(),
                                record.description().to_string(),
                                record.started_at_ms(),
                                record.publication_revision(),
                            )
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .expect("issue active-adoption receipt")
            .expect("persistent registries issue receipts");
        let second_digest = registry
            .with_active_main_session_adoption(|receipt| receipt.digest())
            .expect("reissue active-adoption receipt")
            .expect("persistent registries issue receipts");

        assert_eq!(observed.0, session_id);
        assert_eq!(observed.1, 8);
        assert_eq!(observed.2, second_digest);
        assert_eq!(
            observed.3,
            vec![
                (
                    "legacy-a-running".to_string(),
                    "active legacy-a-running".to_string(),
                    Some(20),
                    2,
                ),
                (
                    "legacy-z-running".to_string(),
                    "active legacy-z-running".to_string(),
                    Some(20),
                    7,
                ),
            ]
        );
        assert!(
            TaskRegistry::new("process-local-active-adoption".to_string())
                .with_active_main_session_adoption(|_| ())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn persistent_typed_provider_outcome_round_trips_budget_terminal() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tasks");
        let session_id = "provider-budget-terminal".to_string();
        let registry = TaskRegistry::new_persistent(session_id.clone(), root.clone()).unwrap();
        let task = registry.create_main_session("background provider".to_string());
        registry.mark_running(&task.id).unwrap();
        let terminal = orca_core::budget::OperationTerminal::Stopped {
            reason: orca_core::budget::StopReason::WallTimeBudget {
                max_wall_time_ms: 1_000,
            },
            usage: orca_core::budget::BudgetUsage {
                turns: 2,
                tool_calls: 1,
                cost_usd_micros: 50,
                wall_time_ms: 1_001,
            },
            checkpoint_id: "provider-budget-stop".to_string(),
            resumable: false,
        };
        registry
            .record_typed_provider_outcome(
                &task.id,
                DurableTypedProviderOutcome {
                    status: TaskStatus::Stopped,
                    response: None,
                    error: Some("budget stopped: wall_time_budget".to_string()),
                    usage: None,
                    operation_terminal: Some(terminal.clone()),
                    completed_at_ms: 42,
                },
            )
            .unwrap();

        let reloaded = TaskRegistry::new_persistent_attached(session_id, root).unwrap();
        let outcome = reloaded
            .typed_provider_outcome(&task.id)
            .expect("durable provider outcome reloads");
        assert_eq!(outcome.status, TaskStatus::Stopped);
        assert_eq!(outcome.operation_terminal, Some(terminal));
    }

    fn runtime_response(response: ProviderResponse) -> RuntimeModelResponse {
        RuntimeModelResponse::new(response, TurnId::new())
    }

    #[test]
    fn concurrent_persistent_sessions_merge_task_index_updates() {
        let root = tempfile::tempdir().unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(5));

        std::thread::scope(|scope| {
            for worker in 0..4 {
                let barrier = Arc::clone(&barrier);
                let root = root.path().to_path_buf();
                scope.spawn(move || {
                    let registry =
                        TaskRegistry::new_persistent(format!("concurrent-session-{worker}"), root)
                            .unwrap();
                    barrier.wait();
                    registry.create_main_session(format!("concurrent task {worker}"));
                });
            }
            barrier.wait();
        });

        let index = TaskPersistence::new(root.path().to_path_buf(), String::new())
            .load_index()
            .unwrap();
        assert_eq!(
            index.len(),
            4,
            "concurrent task index lost updates: {index:?}"
        );
        for worker in 0..4 {
            assert!(
                index
                    .values()
                    .any(|session| session == &format!("concurrent-session-{worker}")),
                "missing concurrent session {worker}: {index:?}"
            );
        }
    }

    #[test]
    fn process_local_workflow_storage_is_shared_and_explicitly_released() {
        let registry = TaskRegistry::new("ephemeral-thread".to_string());
        let clone = registry.clone();
        let cwd = tempfile::tempdir().unwrap();
        let session_dir = registry.workflow_session_dir(cwd.path()).unwrap();
        fs::create_dir_all(&session_dir).unwrap();
        fs::write(session_dir.join("state.json"), "ephemeral state").unwrap();

        assert!(registry.is_process_local());
        assert_eq!(clone.workflow_session_dir(cwd.path()).unwrap(), session_dir);
        assert!(session_dir.join("state.json").is_file());

        clone.release_process_local_artifacts();
        assert!(!session_dir.exists());
        let replacement = registry.workflow_session_dir(cwd.path()).unwrap();
        assert_ne!(replacement, session_dir);
        registry.release_process_local_artifacts();
        assert!(!replacement.exists());
    }

    #[test]
    fn recorded_workflow_storage_preserves_workspace_path() {
        let cwd = tempfile::tempdir().unwrap();
        let registry = TaskRegistry::new_for_cwd("recorded-thread".to_string(), cwd.path());

        assert!(!registry.is_process_local());
        assert_eq!(
            registry.workflow_session_dir(cwd.path()).unwrap(),
            cwd.path()
                .join(".orca")
                .join("workflow-sessions")
                .join("recorded-thread")
        );
    }

    #[test]
    fn legacy_pending_provider_response_gets_one_typed_migration_identity() {
        let response = PersistedProviderResponse {
            steps: Vec::new(),
            assistant_content: Some("legacy pending response".to_string()),
            assistant_reasoning: None,
            tool_calls: Vec::new(),
            usage: None,
            identity: None,
        }
        .into_runtime_response_with_migration_identity();

        assert!(TurnId::parse(response.identity.turn_id.to_string()).is_ok());
        assert_ne!(
            response.identity.item_ids.agent_message_item_id(),
            &response.identity.item_ids.plan_item_id
        );
        assert_ne!(
            response.identity.item_ids.agent_message_item_id(),
            &response.identity.item_ids.reasoning_item_id
        );
    }

    #[test]
    fn persistent_registry_recovers_interrupted_subagent_task_by_id() {
        let temp = tempfile::tempdir().unwrap();
        let registry =
            TaskRegistry::new_persistent("session-1".to_string(), temp.path().join("tasks"))
                .unwrap();
        let task =
            registry.create_subagent("inspect auth".to_string(), Some("general".to_string()));
        registry.mark_running(&task.id).unwrap();

        let reloaded =
            TaskRegistry::new_persistent("session-2".to_string(), temp.path().join("tasks"))
                .unwrap();
        let recovered = reloaded.get(&task.id).expect("persistent task record");

        assert_eq!(recovered.task_type, TaskType::Subagent);
        assert_eq!(recovered.status, TaskStatus::Failed);
        assert_eq!(recovered.description, "inspect auth");
        assert_eq!(recovered.agent_type.as_deref(), Some("general"));
        assert!(
            recovered
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("interrupted before completion")
        );
        assert!(recovered.completed_at_ms.is_some());
    }

    #[test]
    fn persistent_registry_recovers_active_task_with_unmatched_partial_continuation() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tasks");
        let session_id = "partial-continuation-recovery".to_string();
        let registry = TaskRegistry::new_persistent(session_id.clone(), root.clone()).unwrap();
        let task = registry.create_subagent("resume partial state".to_string(), None);
        registry.mark_running(&task.id).unwrap();
        registry
            .with_tasks(|tasks| {
                tasks
                    .get_mut(&task.id)
                    .expect("task record")
                    .continuation_id = Some(AgentContinuationId::new());
                registry
                    .persist_current_task(tasks, &task.id)
                    .expect("persist partial continuation");
            })
            .expect("task registry lock");
        drop(registry);

        let reloaded = TaskRegistry::new_persistent(session_id, root).unwrap();
        let recovered = reloaded.get(&task.id).expect("persistent task record");

        assert_eq!(recovered.status, TaskStatus::Failed);
        assert!(
            recovered
                .error
                .as_deref()
                .is_some_and(|error| error.contains("interrupted before completion"))
        );
        assert!(recovered.completed_at_ms.is_some());
    }

    #[test]
    fn persistent_registry_keeps_worker_owned_subagent_active() {
        let temp = tempfile::tempdir().unwrap();
        let registry =
            TaskRegistry::new_persistent("session-1".to_string(), temp.path().join("tasks"))
                .unwrap();
        let task =
            registry.create_subagent("inspect auth".to_string(), Some("general".to_string()));
        registry.mark_running(&task.id).unwrap();
        registry.mark_worker_spawned(&task.id, 12345).unwrap();

        let reloaded =
            TaskRegistry::new_persistent("session-2".to_string(), temp.path().join("tasks"))
                .unwrap();
        let recovered = reloaded.get(&task.id).expect("persistent task record");

        assert_eq!(recovered.status, TaskStatus::Running);
        assert_eq!(recovered.error, None);
        assert_eq!(recovered.worker_pid, Some(12345));
        assert_eq!(recovered.completed_at_ms, None);
    }

    #[test]
    fn persistent_task_lease_rejects_second_live_owner_and_publishes_revision() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tasks");
        let owner = TaskRegistry::new_persistent("session-1".to_string(), root.clone()).unwrap();
        let task = owner.create_subagent("durable task".to_string(), None);

        let lease = owner.acquire_task_lease(&task.id).unwrap();
        owner.mark_running_with_lease(&lease, &task.id).unwrap();

        let observer =
            TaskRegistry::new_persistent_attached("session-1".to_string(), root).unwrap();
        assert!(matches!(
            observer.acquire_task_lease(&task.id),
            Err(TaskLeaseError::Held { .. })
        ));
        assert_eq!(
            owner.summary(&task.id).unwrap().publication_revision,
            Some(2)
        );
    }

    #[test]
    fn relay_append_is_lease_and_attempt_fenced_before_updating_task_mirror() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tasks");
        let registry =
            TaskRegistry::new_persistent("relay-session".to_string(), root.clone()).unwrap();
        let task = registry.create_subagent("durable activity".to_string(), None);
        let lease = registry.acquire_task_lease(&task.id).unwrap();
        let attempt = AgentAttemptId::new();
        registry
            .mutate_task_for_lease(&task.id, |record| {
                record.continuation_attempt_id = Some(attempt.clone());
                Ok(())
            })
            .unwrap();
        let relay_lease = RelayLease::new(
            &task.id,
            RelayTaskType::Subagent,
            lease.owner_id(),
            lease.epoch(),
            attempt.as_str(),
        )
        .unwrap();
        let mut invalid = RelayRecord::new(
            &relay_lease,
            1,
            test_surface_commit_id(1),
            b"started".to_vec(),
        );
        invalid.payload = b"tampered".to_vec();
        assert!(matches!(
            registry.append_subagent_event_with_lease(&lease, &task.id, attempt.as_str(), invalid,),
            Err(TaskLeaseError::Relay(RelayError::DigestMismatch { .. }))
        ));
        assert_eq!(registry.get(&task.id).unwrap().last_activity_at_ms, None);

        let valid = RelayRecord::new(
            &relay_lease,
            1,
            test_surface_commit_id(1),
            b"started".to_vec(),
        );
        registry
            .append_subagent_event_with_lease(&lease, &task.id, attempt.as_str(), valid.clone())
            .unwrap();
        assert!(
            registry
                .get(&task.id)
                .unwrap()
                .last_activity_at_ms
                .is_some()
        );
        let revision_after_append = registry.get(&task.id).unwrap().publication_revision;
        assert!(matches!(
            registry
                .append_subagent_event_with_lease(&lease, &task.id, attempt.as_str(), valid)
                .unwrap(),
            crate::subagent_event_relay::AppendResult::AlreadyApplied { .. }
        ));
        assert_eq!(
            registry.get(&task.id).unwrap().publication_revision,
            revision_after_append
        );
        let relay = registry
            .open_subagent_event_relay_reader(&task.id, attempt.as_str())
            .unwrap();
        assert_eq!(relay.read_page(0).unwrap().records.len(), 1);
    }

    #[test]
    fn stale_task_lease_cannot_publish_terminal_state_after_takeover() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tasks");
        let first = TaskRegistry::new_persistent("session-1".to_string(), root.clone()).unwrap();
        let task = first.create_subagent("durable task".to_string(), None);
        let stale_lease = first.acquire_task_lease(&task.id).unwrap();

        first
            .mutate_task_for_lease(&task.id, |record| {
                record.lease_expires_at_ms = Some(0);
                Ok(())
            })
            .unwrap();
        let replacement =
            TaskRegistry::new_persistent_attached("session-1".to_string(), root).unwrap();
        let current_lease = replacement.acquire_task_lease(&task.id).unwrap();
        assert!(current_lease.epoch > stale_lease.epoch);

        assert_eq!(
            first
                .complete_with_usage_and_lease(&stale_lease, &task.id, "stale".to_string(), None)
                .unwrap_err(),
            TaskLeaseError::Fenced
        );
        assert_eq!(
            replacement.get(&task.id).unwrap().status,
            TaskStatus::Queued
        );
    }

    #[test]
    fn same_owner_reacquire_after_expiry_fences_its_old_lease() {
        let temp = tempfile::tempdir().unwrap();
        let registry =
            TaskRegistry::new_persistent("session-1".to_string(), temp.path().join("tasks"))
                .unwrap();
        let task = registry.create_subagent("durable task".to_string(), None);
        let old_lease = registry.acquire_task_lease(&task.id).unwrap();
        registry
            .mutate_task_for_lease(&task.id, |record| {
                record.lease_expires_at_ms = Some(0);
                Ok(())
            })
            .unwrap();

        let current_lease = registry.acquire_task_lease(&task.id).unwrap();
        assert!(current_lease.epoch > old_lease.epoch);
        assert_eq!(
            registry
                .complete_with_usage_and_lease(&old_lease, &task.id, "late".to_string(), None)
                .unwrap_err(),
            TaskLeaseError::Fenced
        );
    }

    #[test]
    fn stop_request_revokes_the_current_task_lease_epoch() {
        let temp = tempfile::tempdir().unwrap();
        let registry =
            TaskRegistry::new_persistent("session-1".to_string(), temp.path().join("tasks"))
                .unwrap();
        let task = registry.create_subagent("stoppable task".to_string(), None);
        let lease = registry.acquire_task_lease(&task.id).unwrap();

        registry.request_stop(&task.id).unwrap();

        let record = registry.get(&task.id).unwrap();
        assert!(record.lease_epoch > lease.epoch);
        assert!(record.stop_requested);
        assert!(record.control.cancel.is_cancelled());
        assert_eq!(record.lease_owner, None);
        assert_eq!(
            registry
                .complete_with_usage_and_lease(&lease, &task.id, "late".to_string(), None)
                .unwrap_err(),
            TaskLeaseError::Fenced
        );
    }

    #[test]
    fn refreshed_durable_stop_cancels_the_existing_local_control_token() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tasks");
        let observer =
            TaskRegistry::new_persistent("session-stop-refresh".to_string(), root.clone()).unwrap();
        let task = observer.create_subagent("stoppable task".to_string(), None);
        let local_cancel = observer.get(&task.id).expect("local task").control.cancel;
        let controller =
            TaskRegistry::new_persistent_attached("session-stop-refresh".to_string(), root)
                .unwrap();

        controller.request_stop(&task.id).unwrap();
        assert!(!local_cancel.is_cancelled());

        let refreshed = observer.summary(&task.id).expect("refreshed task");
        assert_eq!(refreshed.status, TaskStatus::Stopping);
        assert!(local_cancel.is_cancelled());
    }

    #[test]
    fn persistent_list_refreshes_tasks_published_by_attached_registry() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tasks");
        let observer = TaskRegistry::new_persistent("session-1".to_string(), root.clone()).unwrap();
        let worker = TaskRegistry::new_persistent_attached("session-1".to_string(), root).unwrap();
        let task = worker.create_subagent("published by worker".to_string(), None);

        assert_eq!(
            observer
                .list()
                .into_iter()
                .map(|task| task.id)
                .collect::<Vec<_>>(),
            vec![task.id]
        );
    }

    #[test]
    fn persistent_summary_refreshes_a_terminal_update_from_another_registry() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tasks");
        let owner = TaskRegistry::new_persistent("session-1".to_string(), root.clone()).unwrap();
        let task = owner.create_subagent("published by worker".to_string(), None);
        let observer =
            TaskRegistry::new_persistent_attached("session-1".to_string(), root).unwrap();
        assert_eq!(
            observer.summary(&task.id).unwrap().status,
            TaskStatus::Queued
        );

        owner
            .complete(&task.id, "completed elsewhere".to_string())
            .unwrap();

        let summary = observer.summary(&task.id).unwrap();
        assert_eq!(summary.status, TaskStatus::Completed);
        assert_eq!(summary.publication_revision, Some(1));
    }

    #[test]
    fn cwd_constructor_migrates_legacy_project_task_sessions_to_orca_home() {
        let cwd = tempfile::tempdir().unwrap();
        // Synchronous registry work only; the exclusive redirect serializes
        // the env switch against every other test and restores on panic.
        crate::history::with_redirected_orca_home("task-migration", |home| {
            let legacy_root = cwd.path().join(".orca").join("task-sessions");
            let legacy =
                TaskRegistry::new_persistent("legacy-session".to_string(), legacy_root).unwrap();
            let task = legacy
                .create_subagent("legacy async task".to_string(), Some("general".to_string()));
            legacy
                .complete(&task.id, "legacy result".to_string())
                .unwrap();
            drop(legacy);

            let registry = TaskRegistry::new_for_cwd("new-session".to_string(), cwd.path());
            let recovered = registry.get(&task.id).expect("legacy task should migrate");

            assert_eq!(recovered.status, TaskStatus::Completed);
            assert_eq!(recovered.result.as_deref(), Some("legacy result"));
            assert!(
                home.join("task-sessions").join("task-index.json").exists(),
                "migrated task index should be written under ORCA_HOME"
            );
        });
    }

    #[test]
    fn cwd_constructor_persists_new_tasks_under_orca_home_without_project_storage() {
        let cwd = tempfile::tempdir().unwrap();
        // Synchronous registry work only; the exclusive redirect serializes
        // the env switch against every other test and restores on panic.
        crate::history::with_redirected_orca_home("task-persist", |home| {
            let registry = TaskRegistry::new_for_cwd("new-session".to_string(), cwd.path());
            registry.create_subagent("new async task".to_string(), Some("general".to_string()));

            assert!(home.join("task-sessions/task-index.json").exists());
            assert!(!cwd.path().join(".orca/task-sessions").exists());
        });
    }

    #[test]
    fn registry_creates_and_lists_workflow_tasks() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_workflow(
            "workflow-run-1".to_string(),
            "audit".to_string(),
            "Audit code".to_string(),
            2,
        );

        assert!(task.id.starts_with("task-"));
        assert_eq!(task.workflow_run_id.as_deref(), Some("workflow-run-1"));

        let list = registry.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].task_type, TaskType::Workflow);
        assert_eq!(list[0].status, TaskStatus::Queued);
        assert_eq!(list[0].name.as_deref(), Some("audit"));
        assert_eq!(list[0].workflow_run_id.as_deref(), Some("workflow-run-1"));
        assert_eq!(list[0].phase_count, Some(2));
    }

    #[test]
    fn registry_creates_and_lists_subagent_tasks() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task =
            registry.create_subagent("inspect auth".to_string(), Some("general".to_string()));

        assert!(task.id.starts_with("task-"));
        assert_eq!(task.task_type, TaskType::Subagent);

        let list = registry.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].task_type, TaskType::Subagent);
        assert_eq!(list[0].status, TaskStatus::Queued);
        assert_eq!(list[0].description, "inspect auth");
        assert_eq!(list[0].agent_type.as_deref(), Some("general"));
        assert!(list[0].created_at_ms > 0);
        assert_eq!(list[0].started_at_ms, None);
        assert_eq!(list[0].completed_at_ms, None);
    }

    #[test]
    fn registry_creates_and_lists_main_session_tasks() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_main_session("Summarize architecture".to_string());

        assert!(task.id.starts_with("task-"));
        assert_eq!(task.task_type, TaskType::MainSession);

        let list = registry.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].task_type, TaskType::MainSession);
        assert_eq!(list[0].status, TaskStatus::Queued);
        assert_eq!(list[0].description, "Summarize architecture");
        assert_eq!(list[0].agent_type.as_deref(), Some("main-session"));
        assert!(list[0].created_at_ms > 0);
        assert_eq!(list[0].started_at_ms, None);
        assert_eq!(list[0].completed_at_ms, None);
    }

    #[test]
    fn registry_marks_running_main_session_backgrounded() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_main_session("Long analysis".to_string());

        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();

        let list = registry.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].task_type, TaskType::MainSession);
        assert_eq!(list[0].status, TaskStatus::Running);
        assert!(list[0].is_backgrounded);
    }

    #[test]
    fn registry_activity_converges_active_and_attention_states() {
        let registry = TaskRegistry::new("activity-summary".to_string());
        let workflow = registry.create_workflow(
            "workflow-run".to_string(),
            "audit".to_string(),
            "Audit".to_string(),
            0,
        );
        registry.mark_running(&workflow.id).unwrap();
        assert_eq!(registry.activity_summary().active_count, 1);
        assert_eq!(registry.activity_summary().attention_count, 0);
        assert!(registry.has_active_tasks());
        assert!(!registry.requires_attention());

        let main = registry.create_main_session("Needs approval".to_string());
        registry.mark_running(&main.id).unwrap();
        registry
            .apply_main_session_terminal_update(
                &main.id,
                MainSessionTerminalUpdate::ApprovalRequired {
                    summary: "approve".to_string(),
                    pending_tool_call: None,
                    pending_provider_response: None,
                },
                None,
            )
            .unwrap();
        assert_eq!(registry.activity_summary().active_count, 1);
        assert_eq!(registry.activity_summary().attention_count, 1);
        assert!(registry.requires_attention());

        registry.stop(&workflow.id, "done".to_string()).unwrap();
        assert_eq!(registry.activity_summary().active_count, 0);
        assert_eq!(registry.activity_summary().attention_count, 1);
        assert!(!registry.has_active_tasks());
        assert!(registry.requires_attention());
    }

    #[test]
    fn registry_marks_backgrounded_main_session_approval_required() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_main_session("Needs a tool".to_string());

        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        registry
            .approval_required(&task.id, "approval_required".to_string())
            .unwrap();

        let record = registry.get(&task.id).unwrap();
        assert_eq!(record.task_type, TaskType::MainSession);
        assert_eq!(record.status, TaskStatus::ApprovalRequired);
        assert!(record.is_backgrounded);
        assert_eq!(record.result.as_deref(), Some("approval_required"));
        assert_eq!(record.error, None);
        assert!(record.completed_at_ms.is_some());
    }

    #[test]
    fn registry_lists_approval_required_tool_name() {
        use orca_core::approval_types::ActionKind;
        use orca_core::task_types::PendingToolCallSummary;

        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_main_session("Needs a tool".to_string());

        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        registry
            .approval_required_for_pending_tool(
                &task.id,
                "approval_required".to_string(),
                Some(PendingToolCallSummary {
                    id: "mock-tool-1".to_string(),
                    name: "task_list".to_string(),
                    action: ActionKind::Read,
                    target: None,
                    arguments: "{}".to_string(),
                }),
            )
            .unwrap();

        let list = registry.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].status, TaskStatus::ApprovalRequired);
        assert_eq!(list[0].tool.as_deref(), Some("task_list"));
        let pending_tool = list[0].pending_tool_call.as_ref().unwrap();
        assert_eq!(pending_tool.id, "mock-tool-1");
        assert_eq!(pending_tool.name, "task_list");
        assert_eq!(pending_tool.action, ActionKind::Read);
        assert_eq!(pending_tool.arguments, "{}");
    }

    #[test]
    fn registry_records_pending_tool_approval_response_once() {
        use orca_core::approval_types::ActionKind;
        use orca_core::task_types::PendingToolCallSummary;

        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_main_session("Needs approval".to_string());

        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        registry
            .approval_required_for_pending_tool(
                &task.id,
                "approval_required".to_string(),
                Some(PendingToolCallSummary {
                    id: "mock-tool-1".to_string(),
                    name: "task_list".to_string(),
                    action: ActionKind::Read,
                    target: None,
                    arguments: "{}".to_string(),
                }),
            )
            .unwrap();

        assert_eq!(
            registry
                .take_pending_tool_approval_response(&task.id)
                .unwrap(),
            None
        );

        registry
            .submit_pending_tool_approval_response(&task.id, true)
            .unwrap();

        assert_eq!(
            registry
                .take_pending_tool_approval_response(&task.id)
                .unwrap(),
            Some(true)
        );
        assert_eq!(
            registry
                .take_pending_tool_approval_response(&task.id)
                .unwrap(),
            None
        );
    }

    #[test]
    fn registry_records_pending_tool_approval_response_by_request_id() {
        use orca_core::approval_types::ActionKind;
        use orca_core::task_types::PendingToolCallSummary;

        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_main_session("Needs approval".to_string());

        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        registry
            .approval_required_for_pending_tool(
                &task.id,
                "approval_required".to_string(),
                Some(PendingToolCallSummary {
                    id: "approval-request-1".to_string(),
                    name: "task_list".to_string(),
                    action: ActionKind::Read,
                    target: None,
                    arguments: "{}".to_string(),
                }),
            )
            .unwrap();

        let resolved_task_id = registry
            .submit_pending_tool_approval_response_by_request_id("approval-request-1", true)
            .unwrap();

        assert_eq!(resolved_task_id, task.id);
        assert!(
            registry
                .submit_pending_tool_approval_response_by_request_id("approval-request-1", false)
                .is_err()
        );
        assert_eq!(
            registry
                .take_pending_tool_approval_response(&task.id)
                .unwrap(),
            Some(true)
        );
    }

    #[test]
    fn registry_finishes_denied_pending_tool_approval() {
        use orca_core::approval_types::ActionKind;
        use orca_core::task_types::PendingToolCallSummary;

        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_main_session("Needs approval".to_string());

        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        registry.mark_worker_spawned(&task.id, 42).unwrap();
        registry
            .approval_required_for_pending_tool(
                &task.id,
                "approval_required".to_string(),
                Some(PendingToolCallSummary {
                    id: "mock-tool-1".to_string(),
                    name: "task_list".to_string(),
                    action: ActionKind::Read,
                    target: None,
                    arguments: "{}".to_string(),
                }),
            )
            .unwrap();
        registry
            .submit_pending_tool_approval_response(&task.id, false)
            .unwrap();

        assert!(
            registry
                .finish_denied_pending_tool_approval(&task.id)
                .unwrap()
        );

        let record = registry.get(&task.id).unwrap();
        assert_eq!(record.status, TaskStatus::Stopped);
        assert_eq!(record.result.as_deref(), Some("Approval denied"));
        assert_eq!(record.error, None);
        assert_eq!(record.tool, None);
        assert_eq!(record.pending_tool_call, None);
        assert_eq!(record.pending_tool_approval_response, None);
        assert_eq!(record.worker_pid, None);
        assert!(record.completed_at_ms.is_some());

        assert!(
            !registry
                .finish_denied_pending_tool_approval(&task.id)
                .unwrap()
        );
    }

    #[test]
    fn registry_takes_approved_pending_provider_response() {
        use orca_core::approval_types::ActionKind;
        use orca_core::provider_types::{ProviderResponse, ProviderStep};
        use orca_core::tool_types::{ToolName, ToolRequest};

        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_main_session("Needs approval".to_string());
        let tool_request = ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::TaskList,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some("{}".to_string()),
        };
        let response = runtime_response(ProviderResponse {
            steps: vec![ProviderStep::ToolCall(tool_request.clone())],
            assistant_content: Some("I need to inspect tasks.".to_string()),
            assistant_reasoning: Some("Need task_list.".to_string()),
            tool_calls: Vec::new(),
            usage: None,
        });
        let expected_identity = response.identity.clone();

        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        registry
            .approval_required_for_pending_provider_response(
                &task.id,
                "approval_required".to_string(),
                response,
            )
            .unwrap();

        let pending = registry.get(&task.id).unwrap();
        assert_eq!(pending.status, TaskStatus::ApprovalRequired);
        assert_eq!(
            pending.pending_tool_call.as_ref().unwrap().id,
            "mock-tool-1"
        );
        assert!(
            registry
                .take_approved_pending_provider_response(&task.id)
                .unwrap()
                .is_none()
        );

        registry
            .submit_pending_tool_approval_response(&task.id, true)
            .unwrap();

        let approved = registry
            .take_approved_pending_provider_response(&task.id)
            .unwrap()
            .expect("approved provider response");
        assert_eq!(
            approved.response.assistant_content.as_deref(),
            Some("I need to inspect tasks.")
        );
        assert_eq!(approved.response.steps.len(), 1);
        assert_eq!(approved.identity, expected_identity);

        let record = registry.get(&task.id).unwrap();
        assert_eq!(record.status, TaskStatus::Running);
        assert_eq!(record.result, None);
        assert_eq!(record.error, None);
        assert_eq!(record.tool, None);
        assert_eq!(record.pending_tool_call, None);
        assert_eq!(record.pending_tool_approval_response, None);
        assert_eq!(record.completed_at_ms, None);

        assert!(
            registry
                .take_approved_pending_provider_response(&task.id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn runtime_continuation_takes_approved_provider_response_with_preapproved_tool_id() {
        use crate::background_turn::take_approved_background_turn_continuation;
        use orca_core::approval_types::ActionKind;
        use orca_core::provider_types::{ProviderResponse, ProviderStep};
        use orca_core::tool_types::{ToolName, ToolRequest};

        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_main_session("Needs approval".to_string());
        let tool_request = ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::TaskList,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some("{}".to_string()),
        };

        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        let response = runtime_response(ProviderResponse {
            steps: vec![ProviderStep::ToolCall(tool_request)],
            assistant_content: Some("I need to inspect tasks.".to_string()),
            assistant_reasoning: Some("Need task_list.".to_string()),
            tool_calls: Vec::new(),
            usage: None,
        });
        let expected_identity = response.identity.clone();
        registry
            .approval_required_for_pending_provider_response(
                &task.id,
                "approval_required".to_string(),
                response,
            )
            .unwrap();
        registry
            .submit_pending_tool_approval_response(&task.id, true)
            .unwrap();

        let continuation = take_approved_background_turn_continuation(&registry, &task.id)
            .unwrap()
            .expect("approved background continuation");

        assert_eq!(
            continuation.preapproved_tool_call_id.as_deref(),
            Some("mock-tool-1")
        );
        assert_eq!(
            continuation.response.response.assistant_content.as_deref(),
            Some("I need to inspect tasks.")
        );
        assert_eq!(continuation.response.identity, expected_identity);
        assert!(
            take_approved_background_turn_continuation(&registry, &task.id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn persistent_registry_restores_pending_provider_response_for_background_continuation() {
        use crate::background_turn::take_approved_background_turn_continuation;
        use orca_core::approval_types::ActionKind;
        use orca_core::provider_types::{ProviderResponse, ProviderStep};
        use orca_core::tool_types::{ToolName, ToolRequest};

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tasks");
        let registry = TaskRegistry::new_persistent("session-1".to_string(), root.clone()).unwrap();
        let task = registry.create_main_session("Needs approval".to_string());
        let tool_request = ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::TaskList,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some("{}".to_string()),
        };

        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        let response = runtime_response(ProviderResponse {
            steps: vec![ProviderStep::ToolCall(tool_request)],
            assistant_content: Some("I need to inspect tasks.".to_string()),
            assistant_reasoning: Some("Need task_list.".to_string()),
            tool_calls: Vec::new(),
            usage: None,
        });
        let expected_identity = response.identity.clone();
        registry
            .approval_required_for_pending_provider_response(
                &task.id,
                "approval_required".to_string(),
                response,
            )
            .unwrap();
        drop(registry);

        let reloaded = TaskRegistry::new_persistent("session-1".to_string(), root).unwrap();
        let pending = reloaded.get(&task.id).expect("persistent task record");
        assert_eq!(pending.status, TaskStatus::ApprovalRequired);
        assert_eq!(
            pending
                .pending_tool_call
                .as_ref()
                .map(|tool| tool.id.as_str()),
            Some("mock-tool-1")
        );

        reloaded
            .submit_pending_tool_approval_response(&task.id, true)
            .unwrap();
        let approved = reloaded
            .get(&task.id)
            .expect("approved persistent task record");
        assert_eq!(approved.status, TaskStatus::ApprovalRequired);
        assert_eq!(approved.pending_tool_approval_response, Some(true));
        assert!(approved.pending_provider_response.is_some());
        let continuation = take_approved_background_turn_continuation(&reloaded, &task.id)
            .unwrap()
            .expect("approved background continuation after reload");

        assert_eq!(
            continuation.preapproved_tool_call_id.as_deref(),
            Some("mock-tool-1")
        );
        assert_eq!(
            continuation.response.response.assistant_content.as_deref(),
            Some("I need to inspect tasks.")
        );
        assert_eq!(continuation.response.response.steps.len(), 1);
        assert_eq!(continuation.response.identity, expected_identity);
    }

    #[test]
    fn persistent_registry_fails_closed_invalid_pending_provider_response() {
        use orca_core::approval_types::ActionKind;
        use orca_core::provider_types::{ProviderResponse, ProviderStep};
        use orca_core::tool_types::{ToolName, ToolRequest};

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tasks");
        let registry = TaskRegistry::new_persistent("session-1".to_string(), root.clone()).unwrap();
        let task = registry.create_main_session("Needs approval".to_string());
        let tool_request = ToolRequest {
            id: "mock-tool-1".to_string(),
            name: ToolName::TaskList,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some("{}".to_string()),
        };

        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        registry
            .approval_required_for_pending_provider_response(
                &task.id,
                "approval_required".to_string(),
                runtime_response(ProviderResponse {
                    steps: vec![ProviderStep::ToolCall(tool_request)],
                    assistant_content: Some("I need to inspect tasks.".to_string()),
                    assistant_reasoning: Some("Need task_list.".to_string()),
                    tool_calls: Vec::new(),
                    usage: None,
                }),
            )
            .unwrap();
        drop(registry);

        let tasks_path = root.join("session-1").join("tasks.json");
        let mut tasks: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&tasks_path).unwrap()).unwrap();
        tasks[&task.id]["pending_provider_response"]["steps"][0]["type"] =
            serde_json::Value::String("future_step".to_string());
        std::fs::write(&tasks_path, serde_json::to_string_pretty(&tasks).unwrap()).unwrap();

        let reloaded = TaskRegistry::new_persistent("session-1".to_string(), root)
            .expect("invalid pending continuation should not prevent registry recovery");
        let recovered = reloaded.get(&task.id).expect("recovered task record");

        assert_eq!(recovered.status, TaskStatus::Failed);
        assert_eq!(recovered.pending_tool_call, None);
        assert_eq!(recovered.pending_tool_approval_response, None);
        assert!(recovered.pending_provider_response.is_none());
        assert!(recovered.completed_at_ms.is_some());
        assert!(
            recovered
                .error
                .as_deref()
                .is_some_and(|error| error.contains("invalid pending provider response")),
            "expected invalid continuation error, got {:?}",
            recovered.error
        );
        let rewritten: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&tasks_path).unwrap()).unwrap();
        assert_eq!(rewritten[&task.id]["status"], serde_json::json!("failed"));
        assert!(rewritten[&task.id]["pending_provider_response"].is_null());
    }

    #[test]
    fn registry_rejects_approval_response_without_pending_tool() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_main_session("No pending tool".to_string());

        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();

        let error = registry
            .submit_pending_tool_approval_response(&task.id, true)
            .expect_err("approval response should require pending tool approval");

        assert!(error.contains("approval_required"), "{error}");
    }

    #[test]
    fn registry_tracks_task_lifecycle_timestamps() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_subagent("inspect auth".to_string(), None);

        registry.mark_running(&task.id).unwrap();
        let running = registry.get(&task.id).unwrap();
        assert!(running.started_at_ms.is_some());
        assert_eq!(running.completed_at_ms, None);

        registry
            .complete(&task.id, "finished audit".to_string())
            .unwrap();
        let completed = registry.list().into_iter().next().unwrap();
        assert_eq!(completed.status, TaskStatus::Completed);
        assert!(completed.started_at_ms.is_some());
        assert!(completed.completed_at_ms.is_some());
    }

    #[test]
    fn registry_lists_workflow_progress() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_workflow(
            "workflow-run-1".to_string(),
            "audit".to_string(),
            "Audit code".to_string(),
            3,
        );

        registry
            .update_workflow_progress(
                &task.id,
                orca_core::task_types::WorkflowTaskProgress {
                    total_agents: 5,
                    running_agents: 2,
                    completed_agents: 2,
                    failed_agents: 1,
                    completed_phases: 1,
                    running_phases: 1,
                    failed_phases: 0,
                },
            )
            .unwrap();

        let list = registry.list();
        assert_eq!(
            list[0].workflow_progress,
            Some(orca_core::task_types::WorkflowTaskProgress {
                total_agents: 5,
                running_agents: 2,
                completed_agents: 2,
                failed_agents: 1,
                completed_phases: 1,
                running_phases: 1,
                failed_phases: 0,
            })
        );
    }

    #[test]
    fn registry_lists_workflow_phase_details() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_workflow(
            "workflow-run-1".to_string(),
            "audit".to_string(),
            "Audit code".to_string(),
            1,
        );
        let phase = WorkflowPhaseTaskSummary {
            name: "scan".to_string(),
            status: orca_core::workflow_types::WorkflowRunStatus::Failed,
            agent_count: 1,
            error: Some("scan failed".to_string()),
            fallback: Some("value".to_string()),
        };

        registry
            .update_workflow_phases(&task.id, vec![phase.clone()])
            .unwrap();

        let list = registry.list();
        assert_eq!(list[0].workflow_phases, vec![phase]);
    }

    #[test]
    fn stop_sets_cancel_flag_and_status() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_workflow(
            "workflow-run-1".to_string(),
            "audit".to_string(),
            "Audit code".to_string(),
            0,
        );

        registry.request_stop(&task.id).unwrap();
        let record = registry.get(&task.id).unwrap();
        assert_eq!(record.status, TaskStatus::Stopping);
        assert!(record.control.cancel.is_cancelled());
    }

    #[test]
    fn request_stop_tree_stops_active_descendants_and_leaves_detached_tasks_running() {
        let registry = TaskRegistry::new("session-1".to_string());
        let root = registry.create_main_session("foreground turn".to_string());
        registry.mark_running(&root.id).unwrap();
        let child = registry.create_subagent_with_parent(
            "owned child".to_string(),
            None,
            Some(root.id.clone()),
        );
        registry.mark_running(&child.id).unwrap();
        let grandchild = registry.create_subagent_with_parent(
            "owned grandchild".to_string(),
            None,
            Some(child.id.clone()),
        );
        registry.mark_running(&grandchild.id).unwrap();
        let detached = registry.create_subagent("detached child".to_string(), None);
        registry.mark_running(&detached.id).unwrap();

        let stopped = registry.request_stop_tree(&root.id).unwrap();

        assert_eq!(
            stopped,
            vec![root.id, child.id.clone(), grandchild.id.clone()]
        );
        assert_eq!(
            registry.get(&grandchild.id).unwrap().status,
            TaskStatus::Stopping
        );
        assert_eq!(
            registry.get(&child.id).unwrap().status,
            TaskStatus::Stopping
        );
        assert_eq!(
            registry.get(&detached.id).unwrap().status,
            TaskStatus::Running
        );
        assert!(!registry.is_cancelled(&detached.id));
    }

    #[test]
    fn persistent_stop_tree_discovers_descendants_created_by_attached_worker() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("tasks");
        let owner =
            TaskRegistry::new_persistent("shared-session".to_string(), root_path.clone()).unwrap();
        let root = owner.create_main_session("foreground turn".to_string());
        owner.mark_running(&root.id).unwrap();
        let detached = owner.create_subagent("detached background".to_string(), None);
        owner.mark_running(&detached.id).unwrap();

        let worker =
            TaskRegistry::new_persistent_attached("shared-session".to_string(), root_path.clone())
                .unwrap();
        assert_eq!(worker.get(&root.id).unwrap().status, TaskStatus::Running);
        let child = worker.create_subagent_with_parent(
            "worker child".to_string(),
            None,
            Some(root.id.clone()),
        );
        worker.mark_running(&child.id).unwrap();
        let grandchild = worker.create_subagent_with_parent(
            "worker grandchild".to_string(),
            None,
            Some(child.id.clone()),
        );
        worker.mark_running(&grandchild.id).unwrap();
        assert_eq!(owner.get(&child.id).unwrap().status, TaskStatus::Running);
        assert!(
            owner.list().iter().any(|task| task.id == grandchild.id),
            "task-wide publication did not expose the worker-created grandchild"
        );

        let stopped = owner.request_stop_tree(&root.id).unwrap();

        assert_eq!(
            stopped,
            vec![root.id, child.id.clone(), grandchild.id.clone()]
        );
        assert_eq!(owner.get(&child.id).unwrap().status, TaskStatus::Stopping);
        assert_eq!(
            owner.get(&grandchild.id).unwrap().status,
            TaskStatus::Stopping
        );
        assert_eq!(owner.get(&detached.id).unwrap().status, TaskStatus::Running);
        let persisted =
            TaskRegistry::new_persistent_attached("shared-session".to_string(), root_path).unwrap();
        assert_eq!(
            persisted.get(&child.id).unwrap().status,
            TaskStatus::Stopping
        );
        assert_eq!(
            persisted.get(&grandchild.id).unwrap().status,
            TaskStatus::Stopping
        );
        assert_eq!(
            persisted.get(&detached.id).unwrap().status,
            TaskStatus::Running
        );
    }

    #[test]
    fn persistent_child_creation_refreshes_cancelled_parent_state() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("tasks");
        let owner =
            TaskRegistry::new_persistent("shared-session".to_string(), root_path.clone()).unwrap();
        let root = owner.create_main_session("foreground turn".to_string());
        owner.mark_running(&root.id).unwrap();
        let worker =
            TaskRegistry::new_persistent_attached("shared-session".to_string(), root_path).unwrap();
        assert_eq!(worker.get(&root.id).unwrap().status, TaskStatus::Running);

        owner.request_stop(&root.id).unwrap();
        let child = worker.create_subagent_with_parent(
            "late worker child".to_string(),
            None,
            Some(root.id),
        );

        let child = worker.get(&child.id).unwrap();
        assert_eq!(child.status, TaskStatus::Stopping);
        assert!(child.control.cancel.is_cancelled());
    }

    #[cfg(unix)]
    #[test]
    fn request_stop_does_not_signal_reused_worker_pid() {
        use std::os::unix::process::CommandExt;

        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_subagent("inspect auth".to_string(), None);
        let mut command = std::process::Command::new("sh");
        command
            .arg("-c")
            .arg("sleep 30")
            .arg0("unrelated-process")
            .process_group(0);
        let mut unrelated = command.spawn().expect("spawn unrelated process");
        registry
            .mark_worker_spawned(&task.id, unrelated.id())
            .unwrap();
        registry.mark_running(&task.id).unwrap();

        registry.request_stop(&task.id).unwrap();

        assert!(
            unrelated.try_wait().unwrap().is_none(),
            "reused PID with a different identity must not be signalled"
        );
        let record = registry.get(&task.id).unwrap();
        assert_eq!(record.status, TaskStatus::Stopped);
        assert_eq!(
            record.result.as_deref(),
            Some("Task stopped; worker already exited")
        );
        orca_tools::process::kill_child_tree(&mut unrelated);
        let _ = unrelated.wait();
    }

    #[cfg(unix)]
    #[test]
    fn recovered_worker_identity_accepts_current_and_legacy_launch_shapes() {
        let agent_id = "task-1234";

        assert!(subagent_worker_command_matches(
            "orca-subagent-worker-task-1234 subagent-worker --agent-id task-1234",
            agent_id
        ));
        assert!(subagent_worker_command_matches(
            "/opt/orca/bin/orca subagent-worker --cwd /tmp --agent-id task-1234 --subagent-depth 1",
            agent_id
        ));
        assert!(!subagent_worker_command_matches(
            "/opt/orca/bin/orca subagent-worker --agent-id task-reused",
            agent_id
        ));
        assert!(!subagent_worker_command_matches(
            "/bin/sh -c 'echo --agent-id task-1234'",
            agent_id
        ));
    }

    #[cfg(unix)]
    #[test]
    fn request_stop_terminates_verified_recovered_worker_group() {
        use std::os::unix::process::CommandExt;
        use std::process::Stdio;
        use std::time::Instant;

        let temp = tempfile::tempdir().unwrap();
        let signal_file = temp.path().join("signals");
        let ready_file = temp.path().join("ready");
        let descendant_ready_file = temp.path().join("descendant-ready");
        let root = temp.path().join("tasks");
        let registry = TaskRegistry::new_persistent("session-1".to_string(), root.clone()).unwrap();
        let task = registry.create_subagent("long-running recovered work".to_string(), None);
        let mut command = std::process::Command::new("sh");
        command
            .env("ORCA_TEST_SIGNAL_FILE", &signal_file)
            .env("ORCA_TEST_READY_FILE", &ready_file)
            .env("ORCA_TEST_DESCENDANT_READY_FILE", &descendant_ready_file)
            .arg("-c")
            .arg(
                r#"
trap 'printf "worker\n" >> "$ORCA_TEST_SIGNAL_FILE"; exit 0' TERM
sh -c 'trap '\''printf "descendant\n" >> "$ORCA_TEST_SIGNAL_FILE"; exit 0'\'' TERM; printf "ready\n" > "$ORCA_TEST_DESCENDANT_READY_FILE"; while :; do :; done' &
while [ ! -e "$ORCA_TEST_DESCENDANT_READY_FILE" ]; do sleep 0.01; done
printf 'ready\n' > "$ORCA_TEST_READY_FILE"
while :; do :; done
"#,
            )
            .arg0(subagent_worker_process_name(&task.id))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let mut child = command.spawn().expect("spawn recovered worker fixture");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !ready_file.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            ready_file.exists(),
            "recovered worker fixture did not start"
        );

        registry.mark_worker_spawned(&task.id, child.id()).unwrap();
        registry.mark_running(&task.id).unwrap();
        let recovered = TaskRegistry::new_persistent("session-2".to_string(), root).unwrap();
        let loaded = recovered.get(&task.id).expect("recovered worker task");
        assert_eq!(loaded.worker_pid, Some(child.id()));

        let stop_result = recovered.request_stop(&task.id);
        if stop_result.is_err() {
            orca_tools::process::kill_child_tree(&mut child);
        }
        let _ = child.wait();
        stop_result.unwrap();

        let stopped = recovered.get(&task.id).unwrap();
        assert_eq!(stopped.status, TaskStatus::Stopped);
        assert_eq!(stopped.worker_pid, None);
        let signals = fs::read_to_string(&signal_file).unwrap_or_default();
        assert!(signals.contains("worker"), "recovered worker missed TERM");
        assert!(
            signals.contains("descendant"),
            "recovered worker descendant missed process-group TERM"
        );
    }

    #[cfg(windows)]
    #[test]
    fn request_stop_terminates_worker_through_reopened_named_job() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tasks");
        let owner = TaskRegistry::new_persistent("session-1".to_string(), root.clone()).unwrap();
        let task = owner.create_subagent("long-running recovered work".to_string(), None);
        let mut command = std::process::Command::new("cmd.exe");
        command
            .args(["/D", "/S", "/C", "ping", "-n", "30", "127.0.0.1"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let broker = orca_core::execution_broker::ExecutionBroker::with_backend(
            orca_core::capability::EnforcementState::Advisory,
            "test-worker-fixture",
        );
        let launched = broker
            .launch_user_trusted_named(
                command,
                format!("test-worker:{}", task.id),
                &root,
                orca_core::capability::CapabilitySet::read_only(),
                &async_worker_job_name(&task.id),
            )
            .expect("spawn Windows recovered worker fixture inside named job");
        let (child, process_job) = (launched.child, launched.process_job);
        let pid = child.id();
        owner
            .adopt_subagent_worker_with_job(&task.id, child, process_job)
            .unwrap();

        let recovered = TaskRegistry::new_persistent("session-2".to_string(), root).unwrap();
        assert_eq!(recovered.get(&task.id).unwrap().worker_pid, Some(pid));

        recovered.request_stop(&task.id).unwrap();

        let stopped = recovered.get(&task.id).unwrap();
        assert_eq!(stopped.status, TaskStatus::Stopped);
        assert_eq!(stopped.worker_pid, None);
    }

    #[cfg(unix)]
    #[test]
    fn adopted_worker_is_reaped_after_natural_exit() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_subagent("quick async work".to_string(), None);
        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .expect("spawn quick worker");
        registry.adopt_subagent_worker(&task.id, child).unwrap();
        let worker = Arc::clone(&registry.get(&task.id).unwrap().control.worker);

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let is_reaped = worker
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .is_none();
            if is_reaped {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "worker Child was not reaped after exit"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn reaper_fails_current_worker_that_exits_without_terminal_publication() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tasks");
        let parent =
            TaskRegistry::new_persistent("session-reaper-failure".to_string(), root.clone())
                .unwrap();
        let task = parent.create_subagent("crashing async work".to_string(), None);
        let child = Command::new("sh")
            .arg("-c")
            .arg("sleep 30")
            .spawn()
            .expect("spawn crashing worker fixture");
        let child_pid = child.id();
        parent.adopt_subagent_worker(&task.id, child).unwrap();

        let mut worker =
            TaskRegistry::new_persistent_attached("session-reaper-failure".to_string(), root)
                .unwrap();
        worker.owner_id = new_task_lease_owner_id(child_pid);
        let lease = worker.acquire_task_lease(&task.id).unwrap();
        worker.mark_running_with_lease(&lease, &task.id).unwrap();
        let child_pid = i32::try_from(child_pid).expect("child PID fits pid_t");
        assert_eq!(unsafe { libc::kill(child_pid, libc::SIGKILL) }, 0);

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let record = parent.get(&task.id).expect("parent task");
            if record.status == TaskStatus::Failed {
                assert_eq!(
                    record.error.as_deref(),
                    Some("async subagent worker exited without a terminal task update")
                );
                assert_eq!(record.worker_pid, None);
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "worker exit did not publish a fenced failure: {record:?}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn stale_worker_exit_fence_cannot_overwrite_takeover_terminal() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tasks");
        let parent =
            TaskRegistry::new_persistent("session-reaper-takeover".to_string(), root.clone())
                .unwrap();
        let task = parent.create_subagent("recoverable async work".to_string(), None);
        parent.mark_worker_spawned(&task.id, 4242).unwrap();
        let mut worker = TaskRegistry::new_persistent_attached(
            "session-reaper-takeover".to_string(),
            root.clone(),
        )
        .unwrap();
        worker.owner_id = new_task_lease_owner_id(4242);
        let old_lease = worker.acquire_task_lease(&task.id).unwrap();
        worker
            .mark_running_with_lease(&old_lease, &task.id)
            .unwrap();
        let stale_fence = parent
            .worker_exit_fence(&task.id, 4242, 0)
            .unwrap()
            .expect("current worker exit fence");

        worker
            .mutate_task_for_lease(&task.id, |record| {
                record.lease_expires_at_ms = Some(0);
                Ok(())
            })
            .unwrap();
        let recovery =
            TaskRegistry::new_persistent_attached("session-reaper-takeover".to_string(), root)
                .unwrap();
        let takeover_lease = recovery.acquire_task_lease(&task.id).unwrap();
        recovery
            .complete_with_usage_and_lease(
                &takeover_lease,
                &task.id,
                "recovered result".to_string(),
                None,
            )
            .unwrap();

        assert!(
            !parent
                .fail_worker_exit_if_current(&task.id, &stale_fence)
                .unwrap()
        );
        let terminal = parent.summary(&task.id).expect("takeover terminal");
        assert_eq!(terminal.status, TaskStatus::Completed);
        assert_eq!(terminal.result.as_deref(), Some("recovered result"));
    }

    #[test]
    fn takeover_lease_cannot_be_mistaken_for_exited_worker_lease() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tasks");
        let parent =
            TaskRegistry::new_persistent("session-reaper-preempted".to_string(), root.clone())
                .unwrap();
        let task = parent.create_subagent("preempted async work".to_string(), None);
        parent.mark_worker_spawned(&task.id, 4242).unwrap();
        let recovery =
            TaskRegistry::new_persistent_attached("session-reaper-preempted".to_string(), root)
                .unwrap();
        let takeover_lease = recovery.acquire_task_lease(&task.id).unwrap();
        recovery
            .mark_running_with_lease(&takeover_lease, &task.id)
            .unwrap();

        assert_eq!(parent.worker_exit_fence(&task.id, 4242, 0).unwrap(), None);
        assert_eq!(
            parent.summary(&task.id).expect("recovered task").status,
            TaskStatus::Running
        );
    }

    #[cfg(unix)]
    #[test]
    fn adopting_subagent_worker_persists_running_state_with_real_pid() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tasks");
        let registry = TaskRegistry::new_persistent("session-1".to_string(), root.clone()).unwrap();
        let task = registry.create_subagent("quick async work".to_string(), None);
        registry.mark_worker_spawned(&task.id, 0).unwrap();
        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg("sleep 0.2")
            .spawn()
            .expect("spawn adopted worker");
        let pid = child.id();

        registry.adopt_subagent_worker(&task.id, child).unwrap();

        let local = registry.get(&task.id).expect("local adopted task");
        assert_eq!(local.status, TaskStatus::Running);
        assert_eq!(local.worker_pid, Some(pid));
        assert!(local.started_at_ms.is_some());

        let reloaded = TaskRegistry::new_persistent("session-1".to_string(), root).unwrap();
        let persisted = reloaded.get(&task.id).expect("persisted adopted task");
        assert_eq!(persisted.status, TaskStatus::Running);
        assert_eq!(persisted.worker_pid, Some(pid));
        assert!(persisted.started_at_ms.is_some());

        registry.request_stop(&task.id).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn adopting_worker_cannot_overwrite_fast_terminal_state() {
        let registry = TaskRegistry::new("session-fast-terminal".to_string());
        let task = registry.create_subagent("fast async work".to_string(), None);
        registry
            .complete(&task.id, "finished before adoption".to_string())
            .unwrap();
        let child = Command::new("sh")
            .arg("-c")
            .arg("sleep 5")
            .spawn()
            .expect("spawn late worker");

        let error = registry
            .adopt_subagent_worker(&task.id, child)
            .expect_err("terminal task must reject late adoption");

        assert!(
            error.contains("cannot adopt_subagent_worker task in Completed state"),
            "unexpected adoption error: {error}"
        );
        let completed = registry.get(&task.id).expect("terminal task");
        assert_eq!(completed.status, TaskStatus::Completed);
        assert_eq!(
            completed.result.as_deref(),
            Some("finished before adoption")
        );
        assert_eq!(completed.worker_pid, None);
    }

    #[cfg(unix)]
    #[test]
    fn parent_reaper_refreshes_async_terminal_state_from_persistence() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tasks");
        let parent =
            TaskRegistry::new_persistent("session-reaper".to_string(), root.clone()).unwrap();
        let task = parent.create_subagent("persisted terminal".to_string(), None);
        parent.mark_worker_spawned(&task.id, 0).unwrap();
        let child = Command::new("sh")
            .arg("-c")
            .arg("sleep 0.2")
            .spawn()
            .expect("spawn adopted worker");
        parent.adopt_subagent_worker(&task.id, child).unwrap();

        let worker = TaskRegistry::new_persistent("session-reaper".to_string(), root).unwrap();
        worker
            .complete(&task.id, "worker persisted terminal".to_string())
            .unwrap();
        assert_eq!(parent.get(&task.id).unwrap().status, TaskStatus::Running);

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let record = parent.get(&task.id).expect("parent task");
            if record.status == TaskStatus::Completed {
                assert_eq!(record.result.as_deref(), Some("worker persisted terminal"));
                assert_eq!(record.worker_pid, None);
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "parent reaper did not observe persisted terminal state"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn complete_stores_result() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_workflow(
            "workflow-run-1".to_string(),
            "audit".to_string(),
            "Audit code".to_string(),
            0,
        );

        registry.complete(&task.id, "done".to_string()).unwrap();
        let record = registry.get(&task.id).unwrap();
        assert_eq!(record.status, TaskStatus::Completed);
        assert_eq!(record.result.as_deref(), Some("done"));
        let summary = registry.list().into_iter().next().unwrap();
        assert_eq!(summary.result.as_deref(), Some("done"));
        assert_eq!(summary.error, None);
    }

    #[test]
    fn complete_with_usage_stores_task_usage() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_subagent("inspect auth".to_string(), None);
        let usage = UsageTotals {
            input_tokens: 120,
            output_tokens: 30,
            cache_tokens: 10,
            estimated_cost_usd: 0.0000252,
        };

        registry
            .complete_with_usage(&task.id, "done".to_string(), Some(usage))
            .unwrap();

        let record = registry.get(&task.id).unwrap();
        assert_eq!(record.usage, Some(usage));
        let summary = registry.list().into_iter().next().unwrap();
        assert_eq!(summary.usage, Some(usage));
    }

    #[test]
    fn subagent_activity_updates_live_task_summary() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task =
            registry.create_subagent("inspect auth".to_string(), Some("general".to_string()));
        registry.mark_running(&task.id).unwrap();

        registry
            .update_subagent_activity(&task.id, "bash: cargo test".to_string(), Some(2), None)
            .unwrap();

        let record = registry.get(&task.id).unwrap();
        assert_eq!(
            record.subagent_current_activity.as_deref(),
            Some("bash: cargo test")
        );
        assert_eq!(record.subagent_turn, Some(2));
        assert!(record.last_activity_at_ms.is_some());

        let summary = registry.list().into_iter().next().unwrap();
        assert_eq!(
            summary.subagent_current_activity.as_deref(),
            Some("bash: cargo test")
        );
        assert_eq!(summary.subagent_turn, Some(2));
        assert!(summary.last_activity_at_ms.is_some());
    }

    #[test]
    fn task_summary_preserves_retry_and_truncation_visibility_after_reload() {
        let root = tempfile::tempdir().unwrap();
        let registry =
            TaskRegistry::new_persistent("retry-visibility".to_string(), root.path().to_path_buf())
                .unwrap();
        let task =
            registry.create_subagent("inspect auth".to_string(), Some("general".to_string()));
        registry.mark_running(&task.id).unwrap();
        registry
            .record_retry(&task.id, "provider timed out")
            .unwrap();
        registry.mark_output_truncated(&task.id).unwrap();

        let reloaded = TaskRegistry::new_persistent_attached(
            "retry-visibility".to_string(),
            root.path().to_path_buf(),
        )
        .unwrap();
        let summary = reloaded.summary(&task.id).expect("reloaded task");
        assert_eq!(summary.retry_count, 1);
        assert!(summary.output_truncated);
        assert_eq!(summary.error.as_deref(), Some("provider timed out"));
        assert!(summary.last_activity_at_ms.is_some());
    }

    #[test]
    fn pause_and_resume_toggle_state() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_workflow(
            "workflow-run-1".to_string(),
            "audit".to_string(),
            "Audit code".to_string(),
            0,
        );

        registry.request_pause(&task.id).unwrap();
        let paused = registry.get(&task.id).unwrap();
        assert_eq!(paused.status, TaskStatus::Paused);
        assert!(paused.control.pause.load(Ordering::SeqCst));

        registry.request_resume(&task.id).unwrap();
        let running = registry.get(&task.id).unwrap();
        assert_eq!(running.status, TaskStatus::Running);
        assert!(!running.control.pause.load(Ordering::SeqCst));
    }

    #[test]
    fn registry_marks_backgrounded_main_session_foregrounded() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_main_session("long prompt".to_string());
        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();

        registry.mark_foregrounded(&task.id).unwrap();

        let record = registry.get(&task.id).unwrap();
        assert_eq!(record.status, TaskStatus::Running);
        assert!(!record.is_backgrounded);
        assert!(record.last_activity_at_ms.is_some());
    }

    #[test]
    fn main_session_terminal_update_reports_background_state_and_rejects_late_foreground() {
        let registry = TaskRegistry::new("session-terminal-background".to_string());
        let task = registry.create_main_session("long prompt".to_string());
        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();

        let transition = registry
            .apply_main_session_terminal_update(
                &task.id,
                MainSessionTerminalUpdate::Completed {
                    result: "done".to_string(),
                },
                None,
            )
            .unwrap();

        assert!(transition.is_backgrounded);
        let error = registry.mark_foregrounded(&task.id).unwrap_err();
        assert!(error.contains("Completed"));
        let record = registry.get(&task.id).unwrap();
        assert_eq!(record.status, TaskStatus::Completed);
        assert!(record.is_backgrounded);
    }

    #[test]
    fn main_session_stop_request_wins_atomic_terminal_settlement() {
        let registry = TaskRegistry::new("session-terminal-stop".to_string());
        let task = registry.create_main_session("long prompt".to_string());
        registry.mark_running(&task.id).unwrap();
        registry.mark_backgrounded(&task.id).unwrap();
        registry.request_stop(&task.id).unwrap();
        let usage = UsageTotals {
            input_tokens: 10,
            output_tokens: 2,
            cache_tokens: 1,
            estimated_cost_usd: 0.01,
        };

        let transition = registry
            .apply_main_session_terminal_update(
                &task.id,
                MainSessionTerminalUpdate::Completed {
                    result: "done".to_string(),
                },
                Some(usage),
            )
            .unwrap();

        assert!(transition.is_backgrounded);
        let record = registry.get(&task.id).unwrap();
        assert_eq!(record.status, TaskStatus::Stopped);
        assert_eq!(record.result.as_deref(), Some("cancelled"));
        assert_eq!(record.usage, Some(usage));
    }

    #[test]
    fn foreground_and_terminal_update_observe_one_atomic_order() {
        for iteration in 0..64 {
            let registry = TaskRegistry::new(format!("session-terminal-race-{iteration}"));
            let task = registry.create_main_session("long prompt".to_string());
            registry.mark_running(&task.id).unwrap();
            registry.mark_backgrounded(&task.id).unwrap();

            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let foreground_registry = registry.clone();
            let foreground_task_id = task.id.clone();
            let foreground_barrier = barrier.clone();
            let foreground = std::thread::spawn(move || {
                foreground_barrier.wait();
                foreground_registry.mark_foregrounded(&foreground_task_id)
            });

            barrier.wait();
            let transition = registry
                .apply_main_session_terminal_update(
                    &task.id,
                    MainSessionTerminalUpdate::Completed {
                        result: "done".to_string(),
                    },
                    None,
                )
                .unwrap();
            let foreground_result = foreground.join().unwrap();

            assert_eq!(
                foreground_result.is_ok(),
                !transition.is_backgrounded,
                "foreground result and terminal snapshot diverged on iteration {iteration}"
            );
            assert_eq!(
                registry.get(&task.id).unwrap().status,
                TaskStatus::Completed
            );
        }
    }

    #[test]
    fn mark_running_updates_status() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_workflow(
            "workflow-run-1".to_string(),
            "audit".to_string(),
            "Audit code".to_string(),
            0,
        );

        registry.mark_running(&task.id).unwrap();
        let record = registry.get(&task.id).unwrap();
        assert_eq!(record.status, TaskStatus::Running);
    }

    #[test]
    fn fail_stores_error() {
        let registry = TaskRegistry::new("session-1".to_string());
        let task = registry.create_workflow(
            "workflow-run-1".to_string(),
            "audit".to_string(),
            "Audit code".to_string(),
            0,
        );

        registry.fail(&task.id, "boom".to_string()).unwrap();
        let record = registry.get(&task.id).unwrap();
        assert_eq!(record.status, TaskStatus::Failed);
        assert_eq!(record.error.as_deref(), Some("boom"));
        let summary = registry.list().into_iter().next().unwrap();
        assert_eq!(summary.result, None);
        assert_eq!(summary.error.as_deref(), Some("boom"));
    }
}
