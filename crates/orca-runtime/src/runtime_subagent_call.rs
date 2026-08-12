use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::thread;

use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_core::event_sink::EventSink;
use orca_core::tool_types::{ToolRequest, ToolResult};
use orca_mcp::McpRegistry;
use serde_json::Value;

use crate::agent_child::{
    ChildAgentExecutor, ChildAgentRequest, ChildAgentRuntime, ChildAgentRuntimeContext,
    run_child_agent,
};
use crate::cost::CostTracker;
use crate::hooks::HookRunner;
use crate::instructions::ProjectInstructions;
use crate::lifecycle::{
    RuntimeSessionLifecycle, RuntimeTaskKind, RuntimeTaskLifecycle, RuntimeTaskStatus,
};
use crate::memory::MemoryBlock;
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
    pub(crate) task_registry: TaskRegistry,
    pub(crate) root_task_id: Option<String>,
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
    mut lifecycle: RuntimeSessionLifecycle,
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
        task_registry,
        root_task_id,
    } = invocation;
    let SubagentRequest {
        description,
        prompt,
        subagent_type,
        model,
        mode: _,
        isolation,
        schema,
        delegation,
    } = request;
    let mut child_config = config.clone();
    if let Some(snapshot) = delegation.as_ref() {
        snapshot.apply_to(&mut child_config, model.clone());
    }
    let worktree_guard = if isolation == SubagentIsolation::Worktree {
        match WorktreeGuard::create(&cwd) {
            Ok(guard) => Some(guard),
            Err(error) => {
                let error = format!("failed to create subagent worktree: {error}");
                return RuntimeSubagentCallOutput {
                    result: ToolResult::failed_after_start(&tool_request, &error, None),
                    tool_request,
                    description,
                    task: Some(started_task.with_status(RuntimeTaskStatus::Failed)),
                    status: RunStatus::Failed,
                    event_output: None,
                    event_error: Some(error),
                    cost_tracker: CostTracker::new(config.model.as_deref()),
                    child_budget_usage: None,
                };
            }
        }
    } else {
        None
    };
    let child_cwd = worktree_guard
        .as_ref()
        .map(WorktreeGuard::path)
        .unwrap_or(&cwd)
        .to_path_buf();
    let child_request = ChildAgentRequest {
        prompt,
        subagent_type,
        model,
        depth: child_depth,
        emit_deltas: false,
        allowed_tools: None,
        tool_policy_label: None,
        workflow_ipc,
    };
    let mut child_events = EventFactory::new(format!("subagent-{}", tool_request.id));
    let mut child_sink = EventSink::new(io::sink(), config.output_format);
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
            executor: child_executor,
        });
        run_child_agent(&child_config, &child_request, &mut runtime)
    }));
    let worktree = worktree_guard.map(WorktreeGuard::finish).transpose();

    match child {
        Ok((child, cost_tracker)) => finish_child_output(
            tool_request,
            description,
            schema.as_ref(),
            child,
            cost_tracker,
            worktree,
            lifecycle,
            started_task,
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
            RuntimeSubagentCallOutput {
                result: ToolResult::indeterminate_after_start(&tool_request, &error),
                tool_request,
                description,
                task: Some(task),
                status: RunStatus::Failed,
                event_output: None,
                event_error: Some(error),
                cost_tracker: CostTracker::new(config.model.as_deref()),
                child_budget_usage: None,
            }
        }
    }
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
