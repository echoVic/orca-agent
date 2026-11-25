use std::cell::{Cell, RefCell};
use std::io;
use std::path::Path;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};
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
use crate::runtime_permission::RuntimePermissionRequestHandler;
use crate::runtime_surface::{
    DisplayText, Sha256Digest, SurfaceCommitId, SurfaceOperationId, SurfaceSubagentId,
    SurfaceSubagentPhase, SurfaceSubagentTerminalStatus, SurfaceTaskId, SurfaceToolCallId,
    SurfaceToolTerminalStatus, TaskRevision, UnixMillis,
};
use crate::tasks::TaskRegistry;
use crate::workflow::ipc::WorkflowIpcContext;
use orca_core::thread_identity::TurnId;

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
    /// Whether this child belongs to a headless exec root (never goal mode).
    pub headless_root: bool,
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
            headless_root: false,
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
    /// This child's own durable task id, when it has one.
    pub child_task_id: Option<&'a str>,
    pub checkpoint_observer: Option<&'a dyn ChildAgentCheckpointSink>,
    /// Owned because child execution may cross a worker boundary. The value
    /// is already scoped to this child attempt by the synchronous invoker.
    pub permission_handler: Option<Arc<dyn RuntimePermissionRequestHandler + Send + Sync>>,
    /// The child loop turn id used when constructing the typed permission
    /// owner. `None` is reserved for legacy/background paths that cannot
    /// safely prompt and therefore fail closed.
    pub turn_id: Option<TurnId>,
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
    /// This child's own durable task id, when it has one.
    ///
    /// `root_task_id` names the tree's owner (for a child, its parent); the
    /// child needs its own identity to read guidance addressed to it.
    pub child_task_id: Option<&'a str>,
    pub checkpoint_observer: Option<&'a dyn ChildAgentCheckpointSink>,
    pub permission_handler: Option<Arc<dyn RuntimePermissionRequestHandler + Send + Sync>>,
    pub turn_id: Option<TurnId>,
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
            child_task_id: context.child_task_id,
            checkpoint_observer: context.checkpoint_observer,
            permission_handler: context.permission_handler,
            turn_id: context.turn_id,
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
        batch_id: String,
        batch_size: u32,
    },
    ChildThreadBound {
        thread_id: crate::runtime_surface::SurfaceThreadId,
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
    /// Stable logical turn identity for this child attempt.  The same value
    /// is passed to the child runtime and permission handler, then persisted
    /// in the surface source cursor so a request cannot be replayed against a
    /// different child turn after reconnect or restart.
    pub turn_id: TurnId,
    pub source_sequence: u64,
    pub occurred_at: UnixMillis,
    pub owner: SubagentActivityOwner,
    pub payload: SubagentActivityPayload,
    pub digest: Sha256Digest,
}

impl SubagentActivityEvent {
    // v3 fences envelopes produced by the pre-canonical digest implementation.
    pub(crate) const SCHEMA_VERSION: u16 = 3;

    pub(crate) fn new(
        task_id: SurfaceTaskId,
        subagent_id: SurfaceSubagentId,
        attempt_id: AgentAttemptId,
        turn_id: TurnId,
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
            turn_id,
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
        turn_id: TurnId,
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
            turn_id,
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
            turn_id: &'a TurnId,
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
            turn_id: &self.turn_id,
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
    pub(crate) turn_id: TurnId,
    pub(crate) owner: SubagentActivityOwner,
}

struct PendingSubagentActivity {
    next_sequence: u64,
    pending: Option<SubagentActivityEvent>,
}

fn is_end(payload: &SubagentActivityPayload) -> bool {
    matches!(payload, SubagentActivityPayload::Completed { .. })
}

/// Assigns source sequence, commit id, timestamp, and digest before calling a
/// typed sink. A failed delivery remains pending so an explicit retry uses the
/// exact same durable source identity instead of reconstructing a new event.
pub(crate) struct ChildAgentActivityEmitter {
    identity: SubagentActivityIdentity,
    sink: Arc<dyn ChildAgentActivitySink>,
    pending: Mutex<PendingSubagentActivity>,
    last_streaming: Mutex<Option<Instant>>,
    published_revision: Arc<AtomicU64>,
    /// Whether the child's end may take the place of a report that was
    /// refused again when sent before it. See `end_takes_a_refused_reports_place`.
    end_takes_a_refused_reports_place: bool,
}

impl ChildAgentActivityEmitter {
    pub(crate) fn new(
        identity: SubagentActivityIdentity,
        sink: Arc<dyn ChildAgentActivitySink>,
    ) -> Self {
        Self::new_with_revision_source(identity, sink, Arc::new(AtomicU64::new(0)))
    }

    pub(crate) fn new_with_revision_source(
        identity: SubagentActivityIdentity,
        sink: Arc<dyn ChildAgentActivitySink>,
        published_revision: Arc<AtomicU64>,
    ) -> Self {
        Self::new_with_revision_source_and_sequence(identity, sink, published_revision, 1)
    }

    /// Constructs an emitter whose first source sequence is already advanced
    /// past an actor-committed event. Detached workers use this when the parent
    /// actor commits `Started` before spawning the process; the worker then
    /// begins at sequence two without duplicating or racing that start fact.
    pub(crate) fn new_with_revision_source_and_sequence(
        identity: SubagentActivityIdentity,
        sink: Arc<dyn ChildAgentActivitySink>,
        published_revision: Arc<AtomicU64>,
        next_sequence: u64,
    ) -> Self {
        assert!(
            next_sequence > 0,
            "activity source sequence must be non-zero"
        );
        Self {
            identity,
            sink,
            pending: Mutex::new(PendingSubagentActivity {
                next_sequence,
                pending: None,
            }),
            last_streaming: Mutex::new(None),
            published_revision,
            end_takes_a_refused_reports_place: false,
        }
    }

    /// For a child kept in its host's process. Once that child's parent turn
    /// is over, its parent takes its end and refuses anything else it
    /// reports, so a report made in between is refused each time it is sent.
    /// Sent first, as a refused report always is, it would keep the end from
    /// ever being sent. With this, the end takes that report's place: the
    /// report is dropped and the end goes out in its sequence, which the
    /// refused report never used.
    pub(crate) fn end_takes_a_refused_reports_place(mut self) -> Self {
        self.end_takes_a_refused_reports_place = true;
        self
    }

    pub(crate) fn revision_source(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.published_revision)
    }

    pub(crate) fn publish_payload(&self, payload: SubagentActivityPayload) -> io::Result<()> {
        let mut pending = self
            .pending
            .lock()
            .map_err(|_| io::Error::other("child activity emitter is unavailable after a panic"))?;
        // A failed sink keeps the exact event pending. If the caller advances
        // to a different payload before retrying, flush the old event first;
        // silently replacing it would create a source-sequence gap and lose
        // the activity that the parent surface is meant to observe.
        if let Some(event) = pending.pending.clone() {
            if event.payload != payload {
                match self.sink.publish(event.clone()) {
                    Ok(()) => {
                        self.published_revision
                            .store(event.source_sequence, Ordering::Release);
                        pending.next_sequence =
                            event.source_sequence.checked_add(1).ok_or_else(|| {
                                io::Error::other("child activity source sequence overflow")
                            })?;
                        pending.pending = None;
                    }
                    // Refused again, and this is a kept child's end: the end
                    // goes in the report's place (see
                    // `end_takes_a_refused_reports_place`). The pending report
                    // never advanced the sequence, so the end takes the
                    // report's own and leaves no gap.
                    Err(_)
                        if self.end_takes_a_refused_reports_place
                            && is_end(&payload)
                            && !is_end(&event.payload) =>
                    {
                        pending.pending = None;
                    }
                    Err(error) => return Err(error),
                }
            } else {
                self.sink.publish(event)?;
                self.published_revision
                    .store(pending.next_sequence, Ordering::Release);
                pending.next_sequence = pending
                    .next_sequence
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("child activity source sequence overflow"))?;
                pending.pending = None;
                return Ok(());
            }
        }
        let next_sequence = pending.next_sequence;
        let event = SubagentActivityEvent::new(
            self.identity.task_id.clone(),
            self.identity.subagent_id.clone(),
            self.identity.attempt_id.clone(),
            self.identity.turn_id.clone(),
            next_sequence,
            self.identity.owner.clone(),
            payload,
        );
        if let Err(error) = self.sink.publish(event.clone()) {
            pending.pending = Some(event);
            return Err(error);
        }
        self.published_revision
            .store(next_sequence, Ordering::Release);
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
                    | RunStatus::VerificationFailed
                    | RunStatus::Blocked { .. } => SurfaceToolTerminalStatus::Failed,
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
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

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
        let turn_id = TurnId::new();
        let emitter = ChildAgentActivityEmitter::new(
            SubagentActivityIdentity {
                task_id: SurfaceTaskId::try_new("task-1").unwrap(),
                subagent_id: SurfaceSubagentId::try_new("subagent-1").unwrap(),
                attempt_id: AgentAttemptId::new(),
                turn_id: turn_id.clone(),
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
            batch_id: "batch".to_string(),
            batch_size: 1,
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
        assert_eq!(events[0].turn_id, turn_id);
        assert_eq!(events[1].turn_id, events[0].turn_id);
        assert_eq!(events[2].turn_id, events[0].turn_id);
        assert!(events[0].verify_digest());
        assert_eq!(events[2].source_sequence, 2);
        assert!(events[2].verify_digest());
    }

    #[test]
    fn typed_emitter_does_not_drop_a_new_payload_after_failure() {
        let sink = Arc::new(RecordingSink {
            fail_once: AtomicBool::new(true),
            events: Mutex::new(Vec::new()),
        });
        let emitter = ChildAgentActivityEmitter::new(
            SubagentActivityIdentity {
                task_id: SurfaceTaskId::try_new("task-2").unwrap(),
                subagent_id: SurfaceSubagentId::try_new("subagent-2").unwrap(),
                attempt_id: AgentAttemptId::new(),
                turn_id: TurnId::new(),
                owner: SubagentActivityOwner::DetachedTask {
                    task_id: SurfaceTaskId::try_new("task-2").unwrap(),
                    task_revision: TaskRevision::try_new(1).unwrap(),
                    authority_digest: Sha256Digest::digest("authority-2"),
                },
            },
            sink.clone(),
        );
        assert!(
            emitter
                .publish_payload(SubagentActivityPayload::Started {
                    description: DisplayText::new("start"),
                    batch_id: "batch".to_string(),
                    batch_size: 1,
                })
                .is_err()
        );
        emitter
            .publish_payload(SubagentActivityPayload::PhaseChanged {
                phase: SurfaceSubagentPhase::Thinking,
                turn: None,
            })
            .unwrap();
        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 3);
        assert!(matches!(
            events[0].payload,
            SubagentActivityPayload::Started { .. }
        ));
        assert!(matches!(
            events[1].payload,
            SubagentActivityPayload::Started { .. }
        ));
        assert!(matches!(
            events[2].payload,
            SubagentActivityPayload::PhaseChanged { .. }
        ));
        assert_eq!(events[0].source_sequence, 1);
        assert_eq!(events[1].source_sequence, 1);
        assert_eq!(events[2].source_sequence, 2);
    }

    #[test]
    fn precommitted_activity_emitter_starts_at_sequence_two() {
        let sink = Arc::new(RecordingSink {
            fail_once: AtomicBool::new(false),
            events: Mutex::new(Vec::new()),
        });
        let turn_id = TurnId::new();
        let emitter = ChildAgentActivityEmitter::new_with_revision_source_and_sequence(
            SubagentActivityIdentity {
                task_id: SurfaceTaskId::try_new("task-precommitted").unwrap(),
                subagent_id: SurfaceSubagentId::try_new("subagent-precommitted").unwrap(),
                attempt_id: AgentAttemptId::new(),
                turn_id: turn_id.clone(),
                owner: SubagentActivityOwner::DetachedTask {
                    task_id: SurfaceTaskId::try_new("task-precommitted").unwrap(),
                    task_revision: TaskRevision::try_new(1).unwrap(),
                    authority_digest: Sha256Digest::digest("authority-precommitted"),
                },
            },
            sink.clone(),
            Arc::new(AtomicU64::new(1)),
            2,
        );
        emitter
            .publish_payload(SubagentActivityPayload::PhaseChanged {
                phase: SurfaceSubagentPhase::Thinking,
                turn: Some(1),
            })
            .unwrap();

        let events = sink.events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source_sequence, 2);
        assert_eq!(events[0].turn_id, turn_id);
    }

    /// Stands in for a kept child's parent. Once `turn_over` is set it takes
    /// only an end and refuses anything else, as the parent does after its
    /// turn is over. With `refuses_ends` set it refuses an end too.
    struct ParentTakingOnlyAnEnd {
        turn_over: AtomicBool,
        refuses_ends: AtomicBool,
        sent: Mutex<Vec<SubagentActivityEvent>>,
        taken: Mutex<Vec<SubagentActivityEvent>>,
    }

    impl ParentTakingOnlyAnEnd {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                turn_over: AtomicBool::new(false),
                refuses_ends: AtomicBool::new(false),
                sent: Mutex::new(Vec::new()),
                taken: Mutex::new(Vec::new()),
            })
        }

        fn taken(&self) -> Vec<(u64, SubagentActivityPayload)> {
            self.taken
                .lock()
                .unwrap()
                .iter()
                .map(|event| (event.source_sequence, event.payload.clone()))
                .collect()
        }

        fn sent(&self) -> Vec<(u64, SubagentActivityPayload)> {
            self.sent
                .lock()
                .unwrap()
                .iter()
                .map(|event| (event.source_sequence, event.payload.clone()))
                .collect()
        }
    }

    impl ChildAgentActivitySink for ParentTakingOnlyAnEnd {
        fn publish(&self, event: SubagentActivityEvent) -> io::Result<()> {
            self.sent.lock().unwrap().push(event.clone());
            let refused = if is_end(&event.payload) {
                self.refuses_ends.load(Ordering::SeqCst)
            } else {
                self.turn_over.load(Ordering::SeqCst)
            };
            if refused {
                return Err(io::Error::new(
                    io::ErrorKind::NotConnected,
                    "runtime generation is not active",
                ));
            }
            self.taken.lock().unwrap().push(event);
            Ok(())
        }
    }

    fn emitter_reporting_to(sink: Arc<ParentTakingOnlyAnEnd>) -> ChildAgentActivityEmitter {
        ChildAgentActivityEmitter::new(
            SubagentActivityIdentity {
                task_id: SurfaceTaskId::try_new("task-kept").unwrap(),
                subagent_id: SurfaceSubagentId::try_new("subagent-kept").unwrap(),
                attempt_id: AgentAttemptId::new(),
                turn_id: TurnId::new(),
                owner: SubagentActivityOwner::Generation {
                    operation_id: SurfaceOperationId::try_from_bytes(
                        *uuid::Uuid::now_v7().as_bytes(),
                    )
                    .unwrap(),
                },
            },
            sink,
        )
    }

    fn a_report() -> SubagentActivityPayload {
        SubagentActivityPayload::ToolStarted {
            call_id: SurfaceToolCallId::try_new("call-1").unwrap(),
            name: "bash".to_string(),
            target: Some(DisplayText::new("sleep 30")),
        }
    }

    fn another_report() -> SubagentActivityPayload {
        SubagentActivityPayload::ToolCompleted {
            call_id: SurfaceToolCallId::try_new("call-1").unwrap(),
            status: SurfaceToolTerminalStatus::Cancelled,
            summary: None,
        }
    }

    fn an_end() -> SubagentActivityPayload {
        SubagentActivityPayload::Completed {
            status: SurfaceSubagentTerminalStatus::Cancelled,
            output: None,
            error: None,
            usage: None,
        }
    }

    fn a_failed_end() -> SubagentActivityPayload {
        SubagentActivityPayload::Completed {
            status: SurfaceSubagentTerminalStatus::Failed,
            output: None,
            error: Some(DisplayText::new("the child failed")),
            usage: None,
        }
    }

    fn thinking() -> SubagentActivityPayload {
        SubagentActivityPayload::PhaseChanged {
            phase: SurfaceSubagentPhase::Thinking,
            turn: Some(1),
        }
    }

    #[test]
    fn a_kept_childs_end_goes_out_in_the_place_of_a_report_refused_again() {
        let parent = ParentTakingOnlyAnEnd::new();
        let emitter = emitter_reporting_to(parent.clone()).end_takes_a_refused_reports_place();
        emitter.publish_payload(thinking()).unwrap();
        parent.turn_over.store(true, Ordering::SeqCst);

        assert!(emitter.publish_payload(a_report()).is_err());
        emitter.publish_payload(an_end()).unwrap();

        // The report was sent again before the end and refused again; the
        // end then went out in the sequence the report never used.
        let sent = parent.sent.lock().unwrap().clone();
        assert_eq!(
            sent.iter()
                .map(|event| (event.source_sequence, event.payload.clone()))
                .collect::<Vec<_>>(),
            vec![
                (1, thinking()),
                (2, a_report()),
                (2, a_report()),
                (2, an_end())
            ]
        );
        assert_eq!(sent[1].surface_commit_id, sent[2].surface_commit_id);
        assert_ne!(sent[3].surface_commit_id, sent[2].surface_commit_id);
        assert!(sent[3].verify_digest());
        assert_eq!(emitter.revision_source().load(Ordering::Acquire), 2);

        // Whatever comes after the end continues from it: no gap, and no
        // sequence used twice.
        parent.turn_over.store(false, Ordering::SeqCst);
        emitter.publish_payload(thinking()).unwrap();
        assert_eq!(
            parent.taken(),
            vec![(1, thinking()), (2, an_end()), (3, thinking())]
        );
        assert_eq!(emitter.revision_source().load(Ordering::Acquire), 3);
    }

    #[test]
    fn an_end_refused_after_taking_a_reports_place_stays_waiting() {
        let parent = ParentTakingOnlyAnEnd::new();
        let emitter = emitter_reporting_to(parent.clone()).end_takes_a_refused_reports_place();
        parent.turn_over.store(true, Ordering::SeqCst);
        parent.refuses_ends.store(true, Ordering::SeqCst);
        assert!(emitter.publish_payload(a_report()).is_err());

        // The end takes the report's place, and is refused too.
        assert!(emitter.publish_payload(an_end()).is_err());
        assert_eq!(
            parent.sent(),
            vec![(1, a_report()), (1, a_report()), (1, an_end())]
        );

        // Now the end is what waits. A report after it sends the end again,
        // and when the end is refused the report is not sent in its place.
        assert!(emitter.publish_payload(another_report()).is_err());
        assert_eq!(
            parent.sent(),
            vec![
                (1, a_report()),
                (1, a_report()),
                (1, an_end()),
                (1, an_end())
            ]
        );
        assert!(parent.taken().is_empty());
        assert_eq!(emitter.revision_source().load(Ordering::Acquire), 0);

        // Once the parent takes an end, the same end goes out, once, in the
        // sequence the dropped report never used.
        parent.refuses_ends.store(false, Ordering::SeqCst);
        emitter.publish_payload(an_end()).unwrap();
        assert_eq!(parent.taken(), vec![(1, an_end())]);
        let sent = parent.sent.lock().unwrap().clone();
        assert_eq!(sent[2].surface_commit_id, sent[4].surface_commit_id);
        assert_eq!(emitter.revision_source().load(Ordering::Acquire), 1);
    }

    #[test]
    fn a_waiting_end_is_not_replaced_by_a_different_end() {
        let parent = ParentTakingOnlyAnEnd::new();
        let emitter = emitter_reporting_to(parent.clone()).end_takes_a_refused_reports_place();
        parent.turn_over.store(true, Ordering::SeqCst);
        parent.refuses_ends.store(true, Ordering::SeqCst);
        assert!(emitter.publish_payload(an_end()).is_err());

        // A different end does not take a refused end's place: the first is
        // sent again, refused again, and the second is not sent.
        assert!(emitter.publish_payload(a_failed_end()).is_err());
        assert_eq!(parent.sent(), vec![(1, an_end()), (1, an_end())]);

        // Once the parent takes ends, the first goes out before the second.
        parent.refuses_ends.store(false, Ordering::SeqCst);
        emitter.publish_payload(a_failed_end()).unwrap();
        assert_eq!(parent.taken(), vec![(1, an_end()), (2, a_failed_end())]);
        assert_eq!(emitter.revision_source().load(Ordering::Acquire), 2);
    }

    #[test]
    fn a_report_taken_when_sent_again_still_goes_before_the_end() {
        let parent = ParentTakingOnlyAnEnd::new();
        let emitter = emitter_reporting_to(parent.clone()).end_takes_a_refused_reports_place();
        parent.turn_over.store(true, Ordering::SeqCst);
        assert!(emitter.publish_payload(a_report()).is_err());

        parent.turn_over.store(false, Ordering::SeqCst);
        emitter.publish_payload(an_end()).unwrap();

        assert_eq!(parent.taken(), vec![(1, a_report()), (2, an_end())]);
    }

    #[test]
    fn a_report_behind_a_refused_report_still_waits_for_it() {
        let parent = ParentTakingOnlyAnEnd::new();
        let emitter = emitter_reporting_to(parent.clone()).end_takes_a_refused_reports_place();
        parent.turn_over.store(true, Ordering::SeqCst);
        assert!(emitter.publish_payload(a_report()).is_err());

        // Not an end: the refused report is sent again, refused again, and
        // this one is not sent at all.
        assert!(emitter.publish_payload(another_report()).is_err());
        let sent = parent.sent.lock().unwrap().clone();
        assert_eq!(
            sent.iter()
                .map(|event| (event.source_sequence, event.payload.clone()))
                .collect::<Vec<_>>(),
            vec![(1, a_report()), (1, a_report())]
        );

        // The refused report is still the one waiting.
        parent.turn_over.store(false, Ordering::SeqCst);
        emitter.publish_payload(another_report()).unwrap();
        assert_eq!(parent.taken(), vec![(1, a_report()), (2, another_report())]);
    }

    #[test]
    fn any_other_childs_end_still_waits_for_a_refused_report() {
        let parent = ParentTakingOnlyAnEnd::new();
        let emitter = emitter_reporting_to(parent.clone());
        parent.turn_over.store(true, Ordering::SeqCst);
        assert!(emitter.publish_payload(a_report()).is_err());

        let refused = emitter
            .publish_payload(an_end())
            .expect_err("the refused report is sent first and refused again");
        assert_eq!(refused.to_string(), "runtime generation is not active");
        assert!(parent.taken().is_empty());
        assert!(
            parent
                .sent
                .lock()
                .unwrap()
                .iter()
                .all(|event| event.payload == a_report())
        );
    }

    #[test]
    fn a_relayed_cost_parses_back_to_the_value_its_digest_covers() {
        // Costs a DeepSeek child reported on 2026-09-23. Without serde_json's
        // exact float parsing each read back one ULP off, so the parent
        // recomputed a different digest and quarantined a healthy relay.
        for estimated_cost_usd in [0.0012501160000000001, 0.00020932800000000003] {
            let event = SubagentActivityEvent::new(
                SurfaceTaskId::try_new("task-cost").unwrap(),
                SurfaceSubagentId::try_new("subagent-cost").unwrap(),
                AgentAttemptId::new(),
                TurnId::new(),
                6,
                SubagentActivityOwner::DetachedTask {
                    task_id: SurfaceTaskId::try_new("task-cost").unwrap(),
                    task_revision: TaskRevision::try_new(1).unwrap(),
                    authority_digest: Sha256Digest::digest("authority-cost"),
                },
                SubagentActivityPayload::Usage {
                    totals: UsageTotals {
                        input_tokens: 3574,
                        output_tokens: 343,
                        cache_tokens: 3072,
                        estimated_cost_usd,
                        service: Default::default(),
                    },
                },
            );
            let relayed: SubagentActivityEvent =
                serde_json::from_slice(&serde_json::to_vec(&event).unwrap()).unwrap();

            assert_eq!(relayed, event, "{estimated_cost_usd} did not round-trip");
            assert!(
                relayed.verify_digest(),
                "{estimated_cost_usd} broke the digest"
            );
        }
    }
}
