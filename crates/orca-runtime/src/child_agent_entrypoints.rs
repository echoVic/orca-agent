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
    ChildAgentActivityPublisher, ChildAgentRequest, ChildAgentResult, ChildAgentRuntime,
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

/// The enforced tool ceiling for a built-in role.
///
/// This is the list that becomes `ChildAgentRequest.allowed_tools`, which both
/// filters the advertised schema and produces a hard rejection for any call
/// outside it. A custom agent has no implicit ceiling; it carries an explicit
/// frozen definition instead.
pub fn role_tool_ceiling(subagent_type: &SubagentType) -> Vec<String> {
    subagent_type
        .builtin()
        .map(|descriptor| {
            descriptor
                .tools
                .iter()
                .map(|tool| (*tool).to_string())
                .collect()
        })
        .unwrap_or_default()
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
    let mut request = request.clone();
    match &request.subagent_type {
        SubagentType::Custom(name) => {
            let definition = child_config.subagents.effective_definition.as_ref();
            let error = match definition {
                Some(definition) if &definition.name == name => definition.validate().err(),
                _ => Some("custom agent requires a matching frozen definition".to_string()),
            };
            if let Some(error) = error {
                return (
                    ChildAgentResult {
                        status: RunStatus::Failed,
                        final_message: None,
                        error: Some(error),
                        budget_usage: None,
                    },
                    CostTracker::new(child_config.model.as_deref()),
                );
            }
            let definition = definition.expect("validated frozen definition");
            let mut allowed = definition.allowed_tools.clone();
            if let Some(ceiling) = &request.allowed_tools {
                allowed.retain(|tool| ceiling.contains(tool));
            }
            request.allowed_tools = Some(allowed);
            request.tool_policy_label = Some(format!("custom agent '{name}'"));
            request.model = definition.model.clone();
        }
        builtin => {
            // A built-in role carries an explicit ceiling from the role catalog,
            // so the advertised schema, the child prompt, and the hard rejection
            // path all read the same list. A caller ceiling can only narrow it.
            let mut allowed = role_tool_ceiling(builtin);
            if matches!(builtin, SubagentType::General) && request.workflow_ipc.is_some() {
                allowed.extend(
                    [
                        "workflow_send_message",
                        "workflow_read_messages",
                        "workflow_clear_messages",
                        "workflow_create_task_list",
                        "workflow_claim_task",
                        "workflow_complete_task",
                        "workflow_list_tasks",
                    ]
                    .into_iter()
                    .map(str::to_owned),
                );
            }
            if let Some(ceiling) = &request.allowed_tools {
                allowed.retain(|tool| ceiling.contains(tool));
            }
            request.allowed_tools = Some(allowed);
            request
                .tool_policy_label
                .get_or_insert_with(|| format!("role '{}' tool policy", builtin.identifier()));
        }
    }
    child_config.model = child_config
        .model
        .with_subagent_override(request.model.clone());
    let mut child_cost_tracker = CostTracker::new(child_config.model.as_deref());
    let result =
        executor(&child_config, &request, &mut child_cost_tracker).unwrap_or_else(|error| {
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
    observer: Option<&dyn ChildAgentActivityPublisher>,
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
