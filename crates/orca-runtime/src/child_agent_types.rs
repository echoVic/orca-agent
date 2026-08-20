use std::cell::{Cell, RefCell};
use std::io;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

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
use crate::runtime_surface::Sha256Digest;
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
        name: String,
        target: Option<String>,
    },
    ToolCompleted {
        name: String,
        status: RunStatus,
    },
    Streaming,
    Usage(UsageTotals),
}

pub struct ChildAgentActivityObserver<'a> {
    emit: RefCell<Box<dyn FnMut(&ChildAgentActivity) + 'a>>,
    last_streaming: Cell<Option<Instant>>,
}

/// The provider fires one `Streaming` activity per SSE delta; consumers fan
/// each activity out to registry writes and channel sends, so per-delta
/// emission must be rate-limited at the source.
const STREAMING_ACTIVITY_INTERVAL: Duration = Duration::from_millis(250);

impl<'a> ChildAgentActivityObserver<'a> {
    pub fn new<F>(emit: F) -> Self
    where
        F: FnMut(&ChildAgentActivity) + 'a,
    {
        Self {
            emit: RefCell::new(Box::new(emit)),
            last_streaming: Cell::new(None),
        }
    }

    pub fn emit(&self, activity: ChildAgentActivity) {
        if matches!(activity, ChildAgentActivity::Streaming) {
            let now = Instant::now();
            let throttled = self
                .last_streaming
                .get()
                .is_some_and(|last| now.duration_since(last) < STREAMING_ACTIVITY_INTERVAL);
            if throttled {
                return;
            }
            self.last_streaming.set(Some(now));
        }
        (self.emit.borrow_mut())(&activity);
    }
}
