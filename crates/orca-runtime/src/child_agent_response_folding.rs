use orca_approval::ApprovalPolicy;
use orca_core::event_schema::RunStatus;
use orca_core::provider_types::{ProviderResponse, ProviderStep};
use orca_core::tool_types::{ToolRequest, ToolResult};
use orca_mcp::McpRegistry;

use crate::agent_common;
use crate::child_agent_loop_setup::ChildAgentLoopSetup;
use crate::child_agent_types::ChildAgentResult;
use crate::cost::CostTracker;
use crate::lifecycle::run_status_from_tool_status;

pub enum ChildAgentProviderResponseFold {
    Complete(ChildAgentResult),
    ContinueToTools,
}

pub enum ChildAgentToolResultFold {
    Continue,
    Stop(ChildAgentResult),
}

pub struct ChildAgentToolExecution {
    pub should_stop: bool,
    pub result: ToolResult,
    pub child_cost: Option<CostTracker>,
}

pub struct ChildAgentToolContext<'a> {
    pub policy: &'a ApprovalPolicy,
    pub mcp_registry: &'a McpRegistry,
}

pub fn fold_child_agent_provider_response(
    setup: &mut ChildAgentLoopSetup,
    response: &ProviderResponse,
    child_cost_tracker: &mut CostTracker,
) -> ChildAgentProviderResponseFold {
    if let Some(usage) = response.usage
        && !usage.is_empty()
    {
        child_cost_tracker.add_usage(usage);
    }

    if response.tool_calls.is_empty() {
        setup.conversation.add_assistant(
            response.assistant_content.clone(),
            response.assistant_reasoning.clone(),
            vec![],
        );
        return ChildAgentProviderResponseFold::Complete(ChildAgentResult {
            status: RunStatus::Success,
            final_message: response.assistant_content.clone(),
            error: None,
            budget_usage: None,
        });
    }

    setup.conversation.add_assistant(
        response.assistant_content.clone(),
        response.assistant_reasoning.clone(),
        response.tool_calls.clone(),
    );
    ChildAgentProviderResponseFold::ContinueToTools
}

pub fn child_agent_tool_requests(response: &ProviderResponse) -> Vec<&ToolRequest> {
    response
        .steps
        .iter()
        .filter_map(|step| match step {
            ProviderStep::ToolCall(request) => Some(request),
            _ => None,
        })
        .collect()
}

pub fn fold_child_agent_tool_result(
    setup: &mut ChildAgentLoopSetup,
    tool_request: &ToolRequest,
    should_stop: bool,
    result: ToolResult,
    child_cost: Option<CostTracker>,
    child_cost_tracker: &mut CostTracker,
) -> ChildAgentToolResultFold {
    debug_assert_eq!(
        tool_request.id, result.id,
        "child tool result must match the requested invocation"
    );
    if let Some(cost) = child_cost {
        child_cost_tracker.merge(&cost);
    }

    let result_content = agent_common::format_tool_result_for_model(&result);
    setup
        .conversation
        .add_tool_result_with_terminal(&result, result_content);

    if should_stop {
        return ChildAgentToolResultFold::Stop(ChildAgentResult {
            status: run_status_from_tool_status(result.status),
            final_message: None,
            error: result.error.clone(),
            budget_usage: None,
        });
    }

    ChildAgentToolResultFold::Continue
}

pub fn fold_child_agent_tool_result_and_close_siblings(
    setup: &mut ChildAgentLoopSetup,
    tool_request: &ToolRequest,
    unstarted_siblings: &[&ToolRequest],
    should_stop: bool,
    result: ToolResult,
    child_cost: Option<CostTracker>,
    child_cost_tracker: &mut CostTracker,
) -> ChildAgentToolResultFold {
    let fold = fold_child_agent_tool_result(
        setup,
        tool_request,
        should_stop,
        result,
        child_cost,
        child_cost_tracker,
    );
    if matches!(fold, ChildAgentToolResultFold::Stop(_)) {
        for sibling in unstarted_siblings {
            let result = ToolResult::cancelled_before_start(
                sibling,
                "an earlier sibling ended the child tool turn",
            );
            let content = agent_common::format_tool_result_for_model(&result);
            setup
                .conversation
                .add_tool_result_with_terminal(&result, content);
        }
    }
    fold
}
