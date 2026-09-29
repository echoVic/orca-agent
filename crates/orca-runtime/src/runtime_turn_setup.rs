use orca_approval::ApprovalPolicy;
use orca_core::config::RunConfig;
use orca_core::subagent_types::SubagentType;
use orca_mcp::McpRegistry;
use orca_provider::{ProviderConfig, context};

use crate::tool_execution::policy_for_tool_execution;
use crate::tool_invocation::{AgentToolPolicyContext, provider_config_for_agent_loop};

pub(crate) struct RuntimeTurnSetupStep;

pub(crate) struct RuntimeTurnSetup {
    pub(crate) context_config: context::ContextConfig,
    pub(crate) policy: ApprovalPolicy,
    pub(crate) provider_config: ProviderConfig,
}

impl RuntimeTurnSetupStep {
    pub(crate) fn new() -> Self {
        Self
    }

    pub(crate) fn prepare(
        &mut self,
        config: &RunConfig,
        subagent_depth: u32,
        subagent_type: &SubagentType,
        tool_policy: AgentToolPolicyContext<'_>,
        mcp_registry: &McpRegistry,
    ) -> RuntimeTurnSetup {
        let budget_model = config.model.as_option();
        let context_config = context::ContextConfig::for_model_with_runtime(
            budget_model.as_deref(),
            &config.model_runtime,
            config.reasoning_effort,
        );
        let policy = policy_for_tool_execution(config);
        let provider_config = provider_config_for_agent_loop(
            config,
            subagent_depth,
            subagent_type,
            tool_policy,
            mcp_registry,
        );

        RuntimeTurnSetup {
            context_config,
            policy,
            provider_config,
        }
    }
}
