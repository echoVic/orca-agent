use orca_approval::ApprovalPolicy;
use orca_core::config::RunConfig;
use orca_core::subagent_types::SubagentType;
use orca_mcp::McpRegistry;
use orca_provider::{ProviderConfig, context};

use crate::tool_execution::policy_for_tool_execution;
use crate::tool_invocation::{AgentToolPolicyContext, provider_config_for_agent_loop};

pub(crate) struct RuntimeTurnSetupStep<'a> {
    session_tools: &'a crate::extension::ExtensionData,
}

pub(crate) struct RuntimeTurnSetup {
    pub(crate) context_config: context::ContextConfig,
    pub(crate) policy: ApprovalPolicy,
    pub(crate) provider_config: ProviderConfig,
}

impl<'a> RuntimeTurnSetupStep<'a> {
    pub(crate) fn new(session_tools: &'a crate::extension::ExtensionData) -> Self {
        Self { session_tools }
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
            self.session_tools,
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
    fn turns_reuse_the_session_agent_catalog_and_new_sessions_see_edits() {
        let home = tempfile::tempdir().unwrap();
        let _home = crate::history::redirect_test_orca_home(home.path());
        let cwd = tempfile::tempdir().unwrap();
        let agents = home.path().join("agents");
        std::fs::create_dir(&agents).unwrap();
        let write_agent = |name: &str, description: &str| {
            std::fs::write(agents.join(format!("{name}.md")), format!(
                "---\nname: {name}\ndescription: {description}\ntools: [read_file]\n---\nRead carefully.\n"
            )).unwrap();
        };
        write_agent("audit", "Review before edit");
        let config = RunConfig {
            cwd: Some(cwd.path().to_path_buf()),
            ..RunConfig::default()
        };
        let mcp = McpRegistry::default();
        let session = crate::extension::ExtensionData::new("session-1");
        let advertised = || {
            let setup = RuntimeTurnSetupStep::new(&session).prepare(
                &config,
                cwd.path(),
                0,
                &SubagentType::General,
                AgentToolPolicyContext::unrestricted(),
                &mcp,
            );
            serde_json::to_vec(&orca_provider::tool_schema::deepseek_tools_schema(
                &setup.provider_config.tools_override.unwrap(),
            ))
            .unwrap()
        };
        let before = advertised();
        assert!(String::from_utf8_lossy(&before).contains("audit: Review before edit"));
        write_agent("audit", "Review after edit");
        write_agent("new_agent", "A new agent");
        assert_eq!(
            before,
            advertised(),
            "a turn must keep the first catalog snapshot"
        );
        let new_session = crate::extension::ExtensionData::new("session-2");
        let fresh = RuntimeTurnSetupStep::new(&new_session).prepare(
            &config,
            cwd.path(),
            0,
            &SubagentType::General,
            AgentToolPolicyContext::unrestricted(),
            &mcp,
        );
        let fresh = serde_json::to_string(&orca_provider::tool_schema::deepseek_tools_schema(
            &fresh.provider_config.tools_override.unwrap(),
        ))
        .unwrap();
        assert!(fresh.contains("audit: Review after edit"));
        assert!(fresh.contains("new_agent: A new agent"));
    }

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
        std::fs::create_dir(child.path().join("metadata")).unwrap();
        std::fs::write(child.path().join(".git"), "gitdir: metadata\n").unwrap();
        let prompt =
            crate::system_prompt::build_system_prompt_for_config(child.path(), &config, 1, None);
        let setup = RuntimeTurnSetupStep::new(&crate::extension::ExtensionData::new("test"))
            .prepare(
                &config,
                child.path(),
                1,
                &SubagentType::General,
                AgentToolPolicyContext::unrestricted(),
                &McpRegistry::default(),
            );

        assert!(!prompt.contains("### git_status\n"));
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
