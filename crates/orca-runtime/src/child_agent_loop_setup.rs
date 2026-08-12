use std::path::Path;

use orca_approval::ApprovalPolicy;
use orca_core::config::RunConfig;
use orca_core::conversation::Conversation;
use orca_core::event_schema::RunStatus;
use orca_mcp::McpRegistry;
use orca_provider::ProviderConfig;
use orca_provider::context::ContextConfig;

use crate::agent_common;
use crate::budget_controller::BudgetLease;
use crate::child_agent_types::{ChildAgentRequest, ChildAgentResult};
use crate::compaction::RuntimeCompactionRetryState;
use crate::instructions::ProjectInstructions;
use crate::memory::MemoryBlock;

pub struct ChildAgentLoopSetup {
    pub mcp_registry: McpRegistry,
    pub provider_config: ProviderConfig,
    pub context_config: ContextConfig,
    pub conversation: Conversation,
    pub policy: ApprovalPolicy,
    pub(crate) turn: u32,
    pub(crate) compaction_retry: RuntimeCompactionRetryState,
}

pub enum ChildAgentTurnBudget {
    Continue,
    Stop(ChildAgentResult),
}

pub fn prepare_child_agent_loop(
    config: &RunConfig,
    request: &ChildAgentRequest,
    cwd: &Path,
    instructions: &ProjectInstructions,
    memory: &MemoryBlock,
) -> ChildAgentLoopSetup {
    let mcp_registry = orca_mcp::initialize_registry(&config.mcp_servers);
    let provider_config = ProviderConfig {
        api_key: config.api_key.clone(),
        base_url: config.base_url.clone(),
        model: Some(orca_core::model::FLASH_MODEL.to_string()),
        reasoning_effort: config.reasoning_effort,
        tools_override: crate::tool_invocation::provider_tool_schema_override(
            request.depth,
            &request.subagent_type,
            crate::tool_invocation::AgentToolPolicyContext::new(
                request.allowed_tools.as_deref(),
                request.tool_policy_label.as_deref(),
            ),
            &mcp_registry,
            &config.external_tools,
        ),
        mcp_registry: Some(mcp_registry.clone()),
        external_tools: config.external_tools.clone(),
    };

    let budget_model = config.model.as_option();
    let context_config =
        ContextConfig::for_model_with_runtime(budget_model.as_deref(), &config.model_runtime);
    let mut conversation = Conversation::new();
    conversation.add_system(agent_common::build_agent_system_prompt(
        cwd,
        request.depth,
        &request.subagent_type,
        Some(instructions),
        config.approval_mode,
        Some(memory),
    ));
    conversation.add_user(request.prompt.clone());

    let policy = ApprovalPolicy::new(config.approval_mode)
        .with_permission_rules(config.permission_rules.clone());

    ChildAgentLoopSetup {
        mcp_registry,
        provider_config,
        context_config,
        conversation,
        policy,
        turn: 0,
        compaction_retry: RuntimeCompactionRetryState::default(),
    }
}

/// Advances the child loop one turn through the child's budget lease. The
/// lease is bounded by the parent's remaining operation budget, so a child
/// can never spend beyond what the parent reserved for it.
pub fn advance_child_agent_turn(
    setup: &mut ChildAgentLoopSetup,
    lease: &mut BudgetLease,
) -> ChildAgentTurnBudget {
    setup.turn = setup.turn.saturating_add(1);
    if let Err(stop) = lease.admit_turn() {
        return ChildAgentTurnBudget::Stop(ChildAgentResult {
            status: RunStatus::Failed,
            final_message: None,
            error: Some(format!(
                "budget stopped: child {} (turns={}, tool_calls={})",
                stop.reason.as_str(),
                stop.usage.turns,
                stop.usage.tool_calls
            )),
        });
    }

    ChildAgentTurnBudget::Continue
}

/// Test-only variant that advances against an explicit turn ceiling without a
/// parent lease.
#[cfg(test)]
pub fn advance_child_agent_turn_with_limit(
    setup: &mut ChildAgentLoopSetup,
    max_turns: u32,
) -> ChildAgentTurnBudget {
    setup.turn = setup.turn.saturating_add(1);
    if setup.turn > max_turns {
        return ChildAgentTurnBudget::Stop(ChildAgentResult {
            status: RunStatus::Failed,
            final_message: None,
            error: Some("budget stopped: child turn budget exhausted".to_string()),
        });
    }

    ChildAgentTurnBudget::Continue
}
