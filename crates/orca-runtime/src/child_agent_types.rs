use std::cell::{Cell, RefCell};
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::cost_types::UsageTotals;
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::event_sink::EventSink;
use orca_core::subagent_types::SubagentType;
use orca_mcp::McpRegistry;

use crate::cost::CostTracker;
use crate::hooks::HookRunner;
use crate::instructions::ProjectInstructions;
use crate::lifecycle::RuntimeSessionLifecycle;
use crate::memory::MemoryBlock;
use crate::tasks::TaskRegistry;
use crate::workflow::ipc::WorkflowIpcContext;

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
}

impl ChildAgentRequest {
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
