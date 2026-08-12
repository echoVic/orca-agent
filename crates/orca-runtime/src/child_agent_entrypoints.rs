use std::io;
use std::path::Path;

use orca_core::cancel::CancelToken;
use orca_core::config::RunConfig;
use orca_core::event_schema::RunStatus;
use orca_core::subagent_types::SubagentType;
use orca_core::tool_types::ToolRequest;

use crate::child_agent_loop_runner::{
    run_child_agent_with_tool_executor, run_child_agent_with_tool_executor_observed,
};
use crate::child_agent_response_folding::{ChildAgentToolContext, ChildAgentToolExecution};
use crate::child_agent_types::{
    ChildAgentActivityObserver, ChildAgentRequest, ChildAgentResult, ChildAgentRuntime,
};
use crate::cost::CostTracker;
use crate::hooks::HookRunner;
use crate::instructions::ProjectInstructions;
use crate::memory::MemoryBlock;

pub struct ChildAgentPromptContext<'a> {
    pub prompt: String,
    pub subagent_type: &'a SubagentType,
    pub subagent_model: Option<String>,
    pub subagent_depth: u32,
    pub cwd: &'a Path,
    pub instructions: &'a ProjectInstructions,
    pub memory: &'a MemoryBlock,
    pub hooks: &'a HookRunner,
}

pub(crate) fn run_child_agent<W: io::Write>(
    config: &RunConfig,
    request: &ChildAgentRequest,
    runtime: &mut ChildAgentRuntime<'_, W>,
) -> (ChildAgentResult, CostTracker) {
    run_child_agent_with_executor(
        config,
        request,
        |child_config, request, child_cost_tracker| {
            runtime.execute(child_config, request, child_cost_tracker)
        },
    )
}

pub fn run_child_agent_with_executor<F>(
    config: &RunConfig,
    request: &ChildAgentRequest,
    mut executor: F,
) -> (ChildAgentResult, CostTracker)
where
    F: FnMut(&RunConfig, &ChildAgentRequest, &mut CostTracker) -> io::Result<ChildAgentResult>,
{
    let mut child_config = config.clone();
    child_config.model = child_config
        .model
        .with_subagent_override(request.model.clone());
    let mut child_cost_tracker = CostTracker::new(child_config.model.as_deref());
    let result =
        executor(&child_config, request, &mut child_cost_tracker).unwrap_or_else(|error| {
            ChildAgentResult {
                status: RunStatus::Failed,
                final_message: None,
                error: Some(error.to_string()),
                budget_usage: None,
            }
        });
    (result, child_cost_tracker)
}

pub fn run_child_agent_prompt_with_tool_executor<F>(
    config: &RunConfig,
    context: ChildAgentPromptContext<'_>,
    execute_tool: F,
) -> (ChildAgentResult, CostTracker)
where
    F: FnMut(
        &RunConfig,
        &ChildAgentRequest,
        &ChildAgentToolContext<'_>,
        &CancelToken,
        &ToolRequest,
    ) -> ChildAgentToolExecution,
{
    let request = ChildAgentRequest::new(
        context.prompt,
        context.subagent_type.clone(),
        context.subagent_model,
        context.subagent_depth,
        false,
    );
    run_child_agent_with_tool_executor(
        config,
        &request,
        context.cwd,
        context.instructions,
        context.memory,
        context.hooks,
        execute_tool,
    )
}

pub fn run_child_agent_prompt_with_tool_executor_observed<F>(
    config: &RunConfig,
    context: ChildAgentPromptContext<'_>,
    observer: Option<&ChildAgentActivityObserver<'_>>,
    execute_tool: F,
) -> (ChildAgentResult, CostTracker)
where
    F: FnMut(
        &RunConfig,
        &ChildAgentRequest,
        &ChildAgentToolContext<'_>,
        &CancelToken,
        &ToolRequest,
    ) -> ChildAgentToolExecution,
{
    let request = ChildAgentRequest::new(
        context.prompt,
        context.subagent_type.clone(),
        context.subagent_model,
        context.subagent_depth,
        false,
    );
    run_child_agent_with_tool_executor_observed(
        config,
        &request,
        context.cwd,
        context.instructions,
        context.memory,
        context.hooks,
        observer,
        execute_tool,
    )
}
