use std::cell::{Cell, RefCell};
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use orca_core::budget::BudgetUsage;
use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::conversation::Conversation;
use orca_core::cost_types::UsageTotals;
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::event_sink::EventSink;
use orca_core::subagent_types::SubagentType;
use orca_mcp::McpRegistry;

use crate::agent_continuation::{
    AgentAttemptId, AgentCheckpoint, AgentContinuationError, AgentContinuationId, AgentPromptId,
    ToolBoundary,
};
use crate::cost::CostTracker;
use crate::hooks::HookRunner;
use crate::instructions::ProjectInstructions;
use crate::lifecycle::RuntimeSessionLifecycle;
use crate::memory::MemoryBlock;
use crate::runtime_surface::{
    DisplayText, Sha256Digest, SurfaceCommitId, SurfaceOperationId, SurfaceSubagentId,
    SurfaceSubagentPhase, SurfaceSubagentTerminalStatus, SurfaceTaskId, SurfaceToolCallId,
    SurfaceToolTerminalStatus, TaskRevision, UnixMillis,
};
use crate::tasks::TaskRegistry;
use crate::workflow::ipc::WorkflowIpcContext;

/// Computed child-runtime identity used to fail closed when a checkpoint was
/// created under a different model, policy, tool catalog, or workspace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ChildAgentCompatibilityIdentity(Sha256Digest);

impl ChildAgentCompatibilityIdentity {
    /// Wraps a compatibility digest computed by the runtime owner; this does
    /// not calculate or validate the hash and has no side effects.
    pub(crate) const fn new(digest: Sha256Digest) -> Self {
        Self(digest)
    }

    /// Returns the already-computed compatibility digest without changing it.
    pub(crate) const fn digest(self) -> Sha256Digest {
        self.0
    }
}

/// Durable data required to start a child loop from a previously committed
/// safe conversation checkpoint.
#[derive(Clone, Debug)]
pub(crate) struct ChildAgentContinuationStart {
    continuation_id: AgentContinuationId,
    attempt_id: AgentAttemptId,
    prompt_id: AgentPromptId,
    checkpoint: AgentCheckpoint,
    compatibility: ChildAgentCompatibilityIdentity,
}

impl ChildAgentContinuationStart {
    /// Creates typed resume startup data from durable identities and a safe
    /// checkpoint; it returns a stable continuation error if the checkpoint
    /// digest is invalid and otherwise changes no external state.
    pub(crate) fn new(
        continuation_id: AgentContinuationId,
        attempt_id: AgentAttemptId,
        prompt_id: AgentPromptId,
        checkpoint: AgentCheckpoint,
        compatibility: ChildAgentCompatibilityIdentity,
    ) -> Result<Self, AgentContinuationError> {
        checkpoint.verify_digest()?;
        Ok(Self {
            continuation_id,
            attempt_id,
            prompt_id,
            checkpoint,
            compatibility,
        })
    }

    pub(crate) fn continuation_id(&self) -> &AgentContinuationId {
        &self.continuation_id
    }

    pub(crate) fn attempt_id(&self) -> &AgentAttemptId {
        &self.attempt_id
    }

    pub(crate) fn prompt_id(&self) -> &AgentPromptId {
        &self.prompt_id
    }

    pub(crate) fn checkpoint(&self) -> &AgentCheckpoint {
        &self.checkpoint
    }

    pub(crate) const fn compatibility(&self) -> ChildAgentCompatibilityIdentity {
        self.compatibility
    }
}

/// Settled child-loop state presented to the runtime-owned checkpoint sink.
pub(crate) struct ChildAgentCheckpointObservation<'a> {
    pub(crate) conversation: &'a Conversation,
    pub(crate) turn: u32,
    pub(crate) usage: BudgetUsage,
    pub(crate) last_tool_boundary: Option<ToolBoundary>,
}

type ChildAgentCheckpointCallback<'a> = dyn for<'checkpoint> FnMut(
        ChildAgentCheckpointObservation<'checkpoint>,
    ) -> Result<(), AgentContinuationError>
    + Send
    + 'a;
type ChildAgentToolBoundaryCallback<'a> =
    dyn FnMut(ToolBoundary) -> Result<(), AgentContinuationError> + Send + 'a;

/// Process-local adapter that keeps checkpoint persistence outside the cloneable
/// child request while supporting both synchronous and worker-owned loops.
pub(crate) struct ChildAgentCheckpointObserver<'a> {
    checkpoint: Mutex<Box<ChildAgentCheckpointCallback<'a>>>,
    tool_boundary: Option<Mutex<Box<ChildAgentToolBoundaryCallback<'a>>>>,
}

pub(crate) trait ChildAgentCheckpointSink {
    fn checkpoint(
        &self,
        observation: ChildAgentCheckpointObservation<'_>,
    ) -> Result<(), AgentContinuationError>;

    fn tool_boundary(&self, _boundary: ToolBoundary) -> Result<(), AgentContinuationError> {
        Ok(())
    }
}

impl<'a> ChildAgentCheckpointObserver<'a> {
    pub(crate) fn new_with_tool_boundary<F, B>(checkpoint: F, tool_boundary: B) -> Self
    where
        F: for<'checkpoint> FnMut(
                ChildAgentCheckpointObservation<'checkpoint>,
            ) -> Result<(), AgentContinuationError>
            + Send
            + 'a,
        B: FnMut(ToolBoundary) -> Result<(), AgentContinuationError> + Send + 'a,
    {
        Self {
            checkpoint: Mutex::new(Box::new(checkpoint)),
            tool_boundary: Some(Mutex::new(Box::new(tool_boundary))),
        }
    }

    /// Sends one settled conversation boundary to the configured sink; it
    /// returns the sink's stable continuation error, or a stable persistence
    /// error if a prior callback panic poisoned the observer, and otherwise
    /// changes state only through that callback.
    pub(crate) fn checkpoint(
        &self,
        observation: ChildAgentCheckpointObservation<'_>,
    ) -> Result<(), AgentContinuationError> {
        let mut checkpoint =
            self.checkpoint
                .lock()
                .map_err(|_| AgentContinuationError::Persistence {
                    message: "child checkpoint observer is unavailable after callback panic"
                        .to_string(),
                })?;
        checkpoint(observation)
    }

    pub(crate) fn tool_boundary(
        &self,
        boundary: ToolBoundary,
    ) -> Result<(), AgentContinuationError> {
        let Some(tool_boundary) = self.tool_boundary.as_ref() else {
            return Ok(());
        };
        let mut callback =
            tool_boundary
                .lock()
                .map_err(|_| AgentContinuationError::Persistence {
                    message: "child tool-boundary observer is unavailable after callback panic"
                        .to_string(),
                })?;
        callback(boundary)
    }
}

impl ChildAgentCheckpointSink for ChildAgentCheckpointObserver<'_> {
    fn checkpoint(
        &self,
        observation: ChildAgentCheckpointObservation<'_>,
    ) -> Result<(), AgentContinuationError> {
        ChildAgentCheckpointObserver::checkpoint(self, observation)
    }

    fn tool_boundary(&self, boundary: ToolBoundary) -> Result<(), AgentContinuationError> {
        ChildAgentCheckpointObserver::tool_boundary(self, boundary)
    }
}

#[derive(Clone, Debug)]
pub struct ChildAgentRequest {
    pub prompt: String,
    pub subagent_type: SubagentType,
    pub model: Option<String>,
    pub depth: u32,
    pub emit_deltas: bool,
    pub allowed_tools: Option<Vec<String>>,
    pub tool_policy_label: Option<String>,
    pub(crate) workflow_ipc: Option<WorkflowIpcContext>,
    pub(crate) continuation: Option<ChildAgentContinuationStart>,
}

impl ChildAgentRequest {
    /// Creates a fresh child request with optional workflow and continuation
    /// startup data disabled; it cannot fail and changes no external state.
    pub fn new(
        prompt: String,
        subagent_type: SubagentType,
        model: Option<String>,
        depth: u32,
        emit_deltas: bool,
    ) -> Self {
        Self {
            prompt,
            subagent_type,
            model,
            depth,
            emit_deltas,
            allowed_tools: None,
            tool_policy_label: None,
            workflow_ipc: None,
            continuation: None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ChildAgentResult {
    pub status: RunStatus,
    pub final_message: Option<String>,
    pub error: Option<String>,
    /// The budget the child actually consumed, when the executing loop owns a
    /// budget (lease or operation controller). The parent merges this receipt
    /// so its own usage reflects every child's consumption.
    pub budget_usage: Option<orca_core::budget::BudgetUsage>,
}

pub(crate) type ChildAgentExecutor<W> = fn(
    &RunConfig,
    &ChildAgentRequest,
    &mut ChildAgentRuntime<'_, W>,
    &mut CostTracker,
) -> io::Result<ChildAgentResult>;

pub(crate) struct ChildAgentRuntime<'a, W: io::Write> {
    pub cwd: &'a Path,
    pub events: &'a mut EventFactory,
    pub sink: &'a mut EventSink<W>,
    pub instructions: &'a ProjectInstructions,
    pub memory: &'a MemoryBlock,
    pub mcp_registry: &'a McpRegistry,
    pub hooks: &'a HookRunner,
    pub cancel: &'a CancelToken,
    pub lifecycle: Option<&'a mut RuntimeSessionLifecycle>,
    pub task_registry: Option<&'a TaskRegistry>,
    pub root_task_id: Option<&'a str>,
    pub checkpoint_observer: Option<&'a dyn ChildAgentCheckpointSink>,
    executor: ChildAgentExecutor<W>,
}

pub(crate) struct ChildAgentRuntimeContext<'a, W: io::Write> {
    pub cwd: &'a Path,
    pub events: &'a mut EventFactory,
    pub sink: &'a mut EventSink<W>,
    pub instructions: &'a ProjectInstructions,
    pub memory: &'a MemoryBlock,
    pub mcp_registry: &'a McpRegistry,
    pub hooks: &'a HookRunner,
    pub cancel: &'a CancelToken,
    pub lifecycle: Option<&'a mut RuntimeSessionLifecycle>,
    pub task_registry: Option<&'a TaskRegistry>,
    pub root_task_id: Option<&'a str>,
    pub checkpoint_observer: Option<&'a dyn ChildAgentCheckpointSink>,
    pub executor: ChildAgentExecutor<W>,
}

impl<'a, W: io::Write> ChildAgentRuntime<'a, W> {
    pub(crate) fn new(context: ChildAgentRuntimeContext<'a, W>) -> Self {
        Self {
            cwd: context.cwd,
            events: context.events,
            sink: context.sink,
            instructions: context.instructions,
            memory: context.memory,
            mcp_registry: context.mcp_registry,
            hooks: context.hooks,
            cancel: context.cancel,
            lifecycle: context.lifecycle,
            task_registry: context.task_registry,
            root_task_id: context.root_task_id,
            checkpoint_observer: context.checkpoint_observer,
            executor: context.executor,
        }
    }

    pub(crate) fn execute(
        &mut self,
        config: &RunConfig,
        request: &ChildAgentRequest,
        child_cost_tracker: &mut CostTracker,
    ) -> io::Result<ChildAgentResult> {
        (self.executor)(config, request, self, child_cost_tracker)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum ChildAgentActivity {
    TurnStarted {
        turn: u32,
    },
    ToolStarted {
        call_id: String,
        name: String,
        target: Option<String>,
    },
    ToolCompleted {
        call_id: String,
        name: String,
        status: RunStatus,
    },
    Streaming,
    Usage(UsageTotals),
}

/// The owner of a child activity stream. This is an identity hint only; the
/// runtime actor still validates the private operation/task fence before
/// committing the event.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
pub(crate) enum SubagentActivityOwner {
    Generation {
        operation_id: SurfaceOperationId,
    },
    DetachedTask {
        task_id: SurfaceTaskId,
        task_revision: TaskRevision,
        authority_digest: Sha256Digest,
    },
}

#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub(crate) enum SubagentActivityPayload {
    Started {
        description: DisplayText,
    },
    PhaseChanged {
        phase: SurfaceSubagentPhase,
        turn: Option<u32>,
    },
    ToolStarted {
        call_id: SurfaceToolCallId,
        name: String,
        target: Option<DisplayText>,
    },
    ToolCompleted {
        call_id: SurfaceToolCallId,
        status: SurfaceToolTerminalStatus,
        summary: Option<DisplayText>,
    },
    Usage {
        totals: UsageTotals,
    },
    CheckpointPublished {
        checkpoint_revision: u64,
    },
    Completed {
        status: SurfaceSubagentTerminalStatus,
        output: Option<DisplayText>,
        error: Option<DisplayText>,
        usage: Option<UsageTotals>,
    },
}

/// Ordered, identity-bound activity emitted by every child execution path.
///
/// `surface_commit_id` is assigned by the source before publication and is
/// reused when a relay or actor retries the same event. `digest` covers every
/// other field, making duplicate delivery safe to recognize and conflicting
/// re-use of a source sequence fail closed.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize)]
pub(crate) struct SubagentActivityEvent {
    pub schema_version: u16,
    pub surface_commit_id: SurfaceCommitId,
    pub task_id: SurfaceTaskId,
    pub subagent_id: SurfaceSubagentId,
    pub attempt_id: AgentAttemptId,
    pub source_sequence: u64,
    pub occurred_at: UnixMillis,
    pub owner: SubagentActivityOwner,
    pub payload: SubagentActivityPayload,
    pub digest: Sha256Digest,
}

impl SubagentActivityEvent {
    pub(crate) const SCHEMA_VERSION: u16 = 1;

    pub(crate) fn new(
        task_id: SurfaceTaskId,
        subagent_id: SurfaceSubagentId,
        attempt_id: AgentAttemptId,
        source_sequence: u64,
        owner: SubagentActivityOwner,
        payload: SubagentActivityPayload,
    ) -> Self {
        let surface_commit_id = SurfaceCommitId::try_from_bytes(*uuid::Uuid::now_v7().as_bytes())
            .expect("UUIDv7 is always a valid surface commit id");
        Self::with_commit_id(
            surface_commit_id,
            task_id,
            subagent_id,
            attempt_id,
            source_sequence,
            owner,
            payload,
        )
    }

    pub(crate) fn with_commit_id(
        surface_commit_id: SurfaceCommitId,
        task_id: SurfaceTaskId,
        subagent_id: SurfaceSubagentId,
        attempt_id: AgentAttemptId,
        source_sequence: u64,
        owner: SubagentActivityOwner,
        payload: SubagentActivityPayload,
    ) -> Self {
        let occurred_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| i64::try_from(duration.as_millis()).ok())
            .map(UnixMillis::new)
            .unwrap_or_else(|| UnixMillis::new(0));
        let mut event = Self {
            schema_version: Self::SCHEMA_VERSION,
            surface_commit_id,
            task_id,
            subagent_id,
            attempt_id,
            source_sequence,
            occurred_at,
            owner,
            payload,
            digest: Sha256Digest::new([0; 32]),
        };
        event.digest = event.compute_digest();
        event
    }

    pub(crate) fn verify_digest(&self) -> bool {
        self.digest == self.compute_digest()
    }

    fn compute_digest(&self) -> Sha256Digest {
        #[derive(serde::Serialize)]
        struct DigestInput<'a> {
            schema_version: u16,
            surface_commit_id: &'a SurfaceCommitId,
            task_id: &'a SurfaceTaskId,
            subagent_id: &'a SurfaceSubagentId,
            attempt_id: &'a AgentAttemptId,
            source_sequence: u64,
            occurred_at: UnixMillis,
            owner: &'a SubagentActivityOwner,
            payload: &'a SubagentActivityPayload,
        }
        let input = DigestInput {
            schema_version: self.schema_version,
            surface_commit_id: &self.surface_commit_id,
            task_id: &self.task_id,
            subagent_id: &self.subagent_id,
            attempt_id: &self.attempt_id,
            source_sequence: self.source_sequence,
            occurred_at: self.occurred_at,
            owner: &self.owner,
            payload: &self.payload,
        };
        Sha256Digest::digest(serde_json::to_vec(&input).expect("activity digest is serializable"))
    }
}

/// Fallible typed activity delivery contract shared by synchronous actor
/// ingress and detached relay writers. Implementations return success only
/// after the event has reached their durable boundary.
pub(crate) trait ChildAgentActivitySink: Send + Sync {
    fn publish(&self, event: SubagentActivityEvent) -> io::Result<()>;
}

/// Identity fixed at the source of one child-attempt activity stream.
#[derive(Clone, Debug)]
pub(crate) struct SubagentActivityIdentity {
    pub(crate) task_id: SurfaceTaskId,
    pub(crate) subagent_id: SurfaceSubagentId,
    pub(crate) attempt_id: AgentAttemptId,
    pub(crate) owner: SubagentActivityOwner,
}

struct PendingSubagentActivity {
    next_sequence: u64,
    pending: Option<SubagentActivityEvent>,
}

/// Assigns source sequence, commit id, timestamp, and digest before calling a
/// typed sink. A failed delivery remains pending so an explicit retry uses the
/// exact same durable source identity instead of reconstructing a new event.
pub(crate) struct ChildAgentActivityEmitter {
    identity: SubagentActivityIdentity,
    sink: Arc<dyn ChildAgentActivitySink>,
    pending: Mutex<PendingSubagentActivity>,
    last_streaming: Mutex<Option<Instant>>,
}

impl ChildAgentActivityEmitter {
    pub(crate) fn new(
        identity: SubagentActivityIdentity,
        sink: Arc<dyn ChildAgentActivitySink>,
    ) -> Self {
        Self {
            identity,
            sink,
            pending: Mutex::new(PendingSubagentActivity {
                next_sequence: 1,
                pending: None,
            }),
            last_streaming: Mutex::new(None),
        }
    }

    pub(crate) fn publish_payload(&self, payload: SubagentActivityPayload) -> io::Result<()> {
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| io::Error::other("child activity emitter is unavailable after a panic"))?;
        let next_sequence = pending.next_sequence;
        let event = pending.pending.get_or_insert_with(|| {
            SubagentActivityEvent::new(
                self.identity.task_id.clone(),
                self.identity.subagent_id.clone(),
                self.identity.attempt_id.clone(),
                next_sequence,
                self.identity.owner.clone(),
                payload,
            )
        });
        self.sink.publish(event.clone())?;
        pending.next_sequence = pending
            .next_sequence
            .checked_add(1)
            .ok_or_else(|| io::Error::other("child activity source sequence overflow"))?;
        pending.pending = None;
        Ok(())
    }

    fn payload_for(activity: ChildAgentActivity) -> SubagentActivityPayload {
        match activity {
            ChildAgentActivity::TurnStarted { turn } => SubagentActivityPayload::PhaseChanged {
                phase: SurfaceSubagentPhase::Thinking,
                turn: Some(turn),
            },
            ChildAgentActivity::Streaming => SubagentActivityPayload::PhaseChanged {
                phase: SurfaceSubagentPhase::Thinking,
                turn: None,
            },
            ChildAgentActivity::ToolStarted {
                call_id,
                name,
                target,
            } => SubagentActivityPayload::ToolStarted {
                call_id: SurfaceToolCallId::try_new(call_id)
                    .expect("child tool call ids are validated before execution"),
                name,
                target: target.map(DisplayText::new),
            },
            ChildAgentActivity::ToolCompleted {
                call_id,
                name: _,
                status,
            } => SubagentActivityPayload::ToolCompleted {
                call_id: SurfaceToolCallId::try_new(call_id)
                    .expect("child tool call ids are validated before execution"),
                status: match status {
                    RunStatus::Success => SurfaceToolTerminalStatus::Success,
                    RunStatus::Cancelled => SurfaceToolTerminalStatus::Cancelled,
                    RunStatus::Failed
                    | RunStatus::ApprovalRequired
                    | RunStatus::VerificationFailed => SurfaceToolTerminalStatus::Failed,
                },
                summary: None,
            },
            ChildAgentActivity::Usage(totals) => SubagentActivityPayload::Usage { totals },
        }
    }
}

/// The loop publishes lightweight facts while an emitter turns them into the
/// typed source envelope. This public adapter preserves the narrow test/tool
/// hook API without allowing it to bypass the production typed sink.
pub trait ChildAgentActivityPublisher {
    fn publish_activity(&self, activity: ChildAgentActivity) -> io::Result<()>;
}

/// Child loops still need an output writer for legacy event formatting. The
/// typed activity observer is attached before this writer, so it is never the
/// delivery boundary for child execution facts.
pub(crate) fn child_event_output() -> io::Sink {
    io::sink()
}

pub struct ChildAgentActivityObserver<'a> {
    emit: RefCell<Box<dyn FnMut(&ChildAgentActivity) -> io::Result<()> + 'a>>,
    last_streaming: Cell<Option<Instant>>,
}

/// The provider fires one `Streaming` activity per SSE delta; consumers fan
/// each activity out to registry writes and channel sends, so per-delta
/// emission must be rate-limited at the source.
const STREAMING_ACTIVITY_INTERVAL: Duration = Duration::from_millis(250);

impl<'a> ChildAgentActivityObserver<'a> {
    pub fn new<F>(mut emit: F) -> Self
    where
        F: FnMut(&ChildAgentActivity) + 'a,
    {
        Self::new_fallible(move |activity| {
            emit(activity);
            Ok(())
        })
    }

    pub fn new_fallible<F>(emit: F) -> Self
    where
        F: FnMut(&ChildAgentActivity) -> io::Result<()> + 'a,
    {
        Self {
            emit: RefCell::new(Box::new(emit)),
            last_streaming: Cell::new(None),
        }
    }

    pub fn emit(&self, activity: ChildAgentActivity) -> io::Result<()> {
        if matches!(activity, ChildAgentActivity::Streaming) {
            let now = Instant::now();
            let throttled = self
                .last_streaming
                .get()
                .is_some_and(|last| now.duration_since(last) < STREAMING_ACTIVITY_INTERVAL);
            if throttled {
                return Ok(());
            }
            self.last_streaming.set(Some(now));
        }
        (self.emit.borrow_mut())(&activity)
    }
}

impl ChildAgentActivityPublisher for ChildAgentActivityObserver<'_> {
    fn publish_activity(&self, activity: ChildAgentActivity) -> io::Result<()> {
        self.emit(activity)
    }
}

impl ChildAgentActivityPublisher for ChildAgentActivityEmitter {
    fn publish_activity(&self, activity: ChildAgentActivity) -> io::Result<()> {
        if matches!(activity, ChildAgentActivity::Streaming) {
            let now = Instant::now();
            let mut last_streaming = self.last_streaming.lock().map_err(|_| {
                io::Error::other("child activity emitter is unavailable after a panic")
            })?;
            if last_streaming
                .is_some_and(|last| now.duration_since(last) < STREAMING_ACTIVITY_INTERVAL)
            {
                return Ok(());
            }
            *last_streaming = Some(now);
        }
        self.publish_payload(Self::payload_for(activity))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    struct RecordingSink {
        fail_once: AtomicBool,
        events: Mutex<Vec<SubagentActivityEvent>>,
    }

    impl ChildAgentActivitySink for RecordingSink {
        fn publish(&self, event: SubagentActivityEvent) -> io::Result<()> {
            self.events.lock().unwrap().push(event);
            if self.fail_once.swap(false, Ordering::SeqCst) {
                return Err(io::Error::other("injected delivery failure"));
            }
            Ok(())
        }
    }

    #[test]
    fn typed_emitter_retries_the_same_stable_event_after_sink_failure() {
        let sink = Arc::new(RecordingSink {
            fail_once: AtomicBool::new(true),
            events: Mutex::new(Vec::new()),
        });
        let emitter = ChildAgentActivityEmitter::new(
            SubagentActivityIdentity {
                task_id: SurfaceTaskId::try_new("task-1").unwrap(),
                subagent_id: SurfaceSubagentId::try_new("subagent-1").unwrap(),
                attempt_id: AgentAttemptId::new(),
                owner: SubagentActivityOwner::DetachedTask {
                    task_id: SurfaceTaskId::try_new("task-1").unwrap(),
                    task_revision: TaskRevision::try_new(1).unwrap(),
                    authority_digest: Sha256Digest::digest("authority"),
                },
            },
            sink.clone(),
        );

        let payload = SubagentActivityPayload::Started {
            description: DisplayText::new("inspect repository"),
        };
        assert!(emitter.publish_payload(payload.clone()).is_err());
        emitter.publish_payload(payload).unwrap();
        emitter
            .publish_payload(SubagentActivityPayload::PhaseChanged {
                phase: SurfaceSubagentPhase::Thinking,
                turn: Some(1),
            })
            .unwrap();

        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0].source_sequence, 1);
        assert_eq!(events[0].surface_commit_id, events[1].surface_commit_id);
        assert_eq!(events[0].digest, events[1].digest);
        assert!(events[0].verify_digest());
        assert_eq!(events[2].source_sequence, 2);
        assert!(events[2].verify_digest());
    }
}
