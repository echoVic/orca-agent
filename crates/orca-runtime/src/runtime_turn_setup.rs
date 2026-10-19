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
        cwd: &std::path::Path,
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
            cwd,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_setup_git_status_uses_effective_child_cwd() {
        let home = tempfile::tempdir().unwrap();
        let _home = crate::history::redirect_test_orca_home(home.path());
        let parent = tempfile::tempdir().unwrap();
        assert!(
            parent
                .path()
                .ancestors()
                .all(|dir| !dir.join(".git").exists())
        );
        let config = RunConfig {
            cwd: Some(parent.path().to_path_buf()),
            ..RunConfig::default()
        };
        let parent_prompt = crate::system_prompt::build_system_prompt(parent.path());
        assert!(parent_prompt.contains("- Git repository: no\n"));

        let child = tempfile::tempdir().unwrap();
        std::fs::write(child.path().join(".git"), "gitdir: /unused/worktree\n").unwrap();
        let prompt =
            crate::system_prompt::build_system_prompt_for_config(child.path(), &config, 1, None);
        let setup = RuntimeTurnSetupStep::new().prepare(
            &config,
            child.path(),
            1,
            &SubagentType::General,
            AgentToolPolicyContext::unrestricted(),
            &McpRegistry::default(),
        );

        assert!(prompt.contains("### git_status\n"));
        assert!(
            setup
                .provider_config
                .tools_override
                .unwrap()
                .iter()
                .any(|tool| tool.name == "git_status")
        );
    }
}
