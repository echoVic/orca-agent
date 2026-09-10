use std::collections::HashSet;

use orca_core::approval_types::{ActionKind, ApprovalRequest};
use orca_core::config::RunConfig;
use orca_core::external_config::ExternalToolConfig;
use orca_core::provider_types::ProviderStep;
use orca_core::subagent_types::SubagentType;
use orca_core::tool_types::{ToolName, ToolRequest, ToolResult};
use orca_mcp::McpRegistry;
use orca_provider::ProviderConfig;
use orca_provider::tool_schema::ProviderToolDefinition;
use orca_tools::schema::{ToolPolicy, canonical_tool_definitions, normalize_tool_request};

use crate::hooks::{HookOutcome, tool_request_with_hook_outcome};

#[derive(Clone, Debug)]
pub struct ToolInvocation {
    pub requested: ToolRequest,
    pub effective: ToolRequest,
    pub action: Option<ActionKind>,
}

#[derive(Clone, Copy)]
pub(crate) struct AgentToolPolicyContext<'a> {
    allowed_tools: Option<&'a [String]>,
    label: Option<&'a str>,
    goal_mode: bool,
}

#[derive(Clone, Debug)]
pub struct ToolExecutionFailure {
    pub request: ToolRequest,
    pub message: String,
}

impl<'a> AgentToolPolicyContext<'a> {
    pub(crate) fn new(allowed_tools: Option<&'a [String]>, label: Option<&'a str>) -> Self {
        Self {
            allowed_tools,
            label,
            goal_mode: false,
        }
    }

    pub(crate) fn unrestricted() -> Self {
        Self::new(None, None)
    }

    pub(crate) fn goal_mode() -> Self {
        Self {
            allowed_tools: None,
            label: None,
            goal_mode: true,
        }
    }

    pub(crate) fn replace_allowed_tools(
        self,
        allowed_tools: Option<&'a [String]>,
        label: &'a str,
    ) -> Self {
        if let Some(allowed_tools) = allowed_tools {
            Self::new(Some(allowed_tools), Some(label))
        } else {
            self
        }
    }

    pub(crate) fn allowed_tools(&self) -> Option<&'a [String]> {
        self.allowed_tools
    }

    pub(crate) fn label(&self) -> Option<&'a str> {
        self.label
    }

    pub(crate) fn is_goal_mode(&self) -> bool {
        self.goal_mode
    }
}

impl ToolExecutionFailure {
    pub fn into_result(self) -> ToolResult {
        ToolResult::invalid_input(&self.request, self.message)
    }
}

pub(crate) fn provider_tool_schema_override(
    subagent_depth: u32,
    subagent_type: &SubagentType,
    tool_policy: AgentToolPolicyContext<'_>,
    mcp_registry: &McpRegistry,
    external_tools: &[ExternalToolConfig],
) -> Option<Vec<ProviderToolDefinition>> {
    let registry = orca_tools::registry::tool_registry_with_mcp_and_external(
        Some(mcp_registry),
        external_tools,
    );
    let policy = if let Some(allowed_tools) = tool_policy.allowed_tools() {
        ToolPolicy::allowed(allowed_tools)
    } else if subagent_depth > 0 {
        ToolPolicy::for_subagent(subagent_type)
    } else if tool_policy.is_goal_mode() {
        ToolPolicy::goal()
    } else {
        ToolPolicy::base()
    };
    Some(
        canonical_tool_definitions(&policy, &registry)
            .into_iter()
            .map(|definition| ProviderToolDefinition {
                name: definition.name,
                description: definition.description,
                input_schema: definition.input_schema,
                strict_capable: definition.strict_capable,
            })
            .collect(),
    )
}

pub(crate) fn provider_config_for_agent_loop(
    config: &RunConfig,
    subagent_depth: u32,
    subagent_type: &SubagentType,
    tool_policy: AgentToolPolicyContext<'_>,
    mcp_registry: &McpRegistry,
) -> ProviderConfig {
    let shell_readiness = crate::shell_readiness::ShellReadiness::for_config(config);
    let mut tools_override = provider_tool_schema_override(
        subagent_depth,
        subagent_type,
        tool_policy,
        mcp_registry,
        &config.external_tools,
    );
    if let Some(tools) = tools_override.as_mut() {
        omit_unavailable_shell_launch_tools(tools, &shell_readiness);
    }
    let mut provider_config = ProviderConfig {
        api_key: config.api_key.clone(),
        base_url: config.base_url.clone(),
        model: config.model.as_option(),
        reasoning_effort: config.reasoning_effort,
        tools_override,
        mcp_registry: Some(mcp_registry.clone()),
        external_tools: config.external_tools.clone(),
    };
    if let Some(tool) = provider_config
        .tools_override
        .as_mut()
        .and_then(|tools| tools.iter_mut().find(|tool| tool.name == "subagent"))
        && let Some(cwd) = config.cwd.clone().or_else(|| std::env::current_dir().ok())
    {
        let catalog = crate::subagent::discover_agents(config, &cwd, mcp_registry);
        orca_tools::schema::apply_subagent_catalog(
            &mut tool.description,
            &mut tool.input_schema,
            &catalog,
        );
    }
    provider_config
}

fn omit_unavailable_shell_launch_tools(
    tools: &mut Vec<ProviderToolDefinition>,
    readiness: &crate::shell_readiness::ShellReadiness,
) {
    tools.retain(|tool| !readiness.blocks_tool_name(&tool.name));
}

pub(crate) fn tool_requests_from_provider_steps(steps: &[ProviderStep]) -> Vec<ToolRequest> {
    steps
        .iter()
        .filter_map(|step| match step {
            ProviderStep::ToolCall(tool_request) => Some(tool_request.clone()),
            _ => None,
        })
        .collect()
}

pub(crate) fn reject_disallowed_child_tool(
    tool_request: &ToolRequest,
    policy: AgentToolPolicyContext<'_>,
    mcp_registry: &McpRegistry,
    external_tools: &[ExternalToolConfig],
) -> Option<ToolResult> {
    child_tool_policy_failure(
        tool_request,
        policy.allowed_tools(),
        policy.label(),
        mcp_registry,
        external_tools,
    )
}

fn child_tool_policy_failure(
    tool_request: &ToolRequest,
    allowed_tools: Option<&[String]>,
    policy_label: Option<&str>,
    mcp_registry: &McpRegistry,
    external_tools: &[ExternalToolConfig],
) -> Option<ToolResult> {
    let allowed_tools = allowed_tools?;
    let registry = orca_tools::registry::tool_registry_with_mcp_and_external(
        Some(mcp_registry),
        external_tools,
    );
    let allowed_canonical_names = allowed_tools
        .iter()
        .filter_map(|tool| {
            registry
                .resolve(tool)
                .map(|resolved| resolved.tool.name().to_string())
        })
        .collect::<HashSet<_>>();
    let requested_name = tool_request.name.as_str();
    let requested_canonical_name = registry
        .resolve(requested_name)
        .map(|resolved| resolved.tool.name().to_string())
        .unwrap_or_else(|| requested_name.to_string());

    if allowed_canonical_names.contains(&requested_canonical_name) {
        return None;
    }

    let label = policy_label.unwrap_or("child agent tool policy");
    Some(ToolResult::invalid_input(
        tool_request,
        format!("{label} disallows tool '{requested_name}'"),
    ))
}

pub fn prepare_tool_invocation(
    tool_request: &ToolRequest,
    subagent_depth: u32,
    mcp_registry: &McpRegistry,
    config: &RunConfig,
) -> ToolInvocation {
    let registry = orca_tools::registry::tool_registry_with_mcp_and_external(
        Some(mcp_registry),
        &config.external_tools,
    );
    let effective =
        normalize_tool_request(&registry, tool_request).unwrap_or_else(|_| tool_request.clone());
    let action =
        if effective.name == ToolName::Subagent && subagent_depth >= config.subagents.max_depth {
            None
        } else {
            Some(orca_tools::canonical_action_kind_with_mcp_and_external(
                &effective,
                Some(mcp_registry),
                &config.external_tools,
            ))
        };

    ToolInvocation {
        requested: tool_request.clone(),
        effective,
        action,
    }
}

pub fn prepare_tool_invocation_with_external(
    tool_request: &ToolRequest,
    subagent_depth: u32,
    max_subagent_depth: u32,
    mcp_registry: &McpRegistry,
    external_tools: &[ExternalToolConfig],
) -> ToolInvocation {
    let registry = orca_tools::registry::tool_registry_with_mcp_and_external(
        Some(mcp_registry),
        external_tools,
    );
    let effective =
        normalize_tool_request(&registry, tool_request).unwrap_or_else(|_| tool_request.clone());
    let action = if effective.name == ToolName::Subagent && subagent_depth >= max_subagent_depth {
        None
    } else {
        Some(orca_tools::canonical_action_kind_with_mcp_and_external(
            &effective,
            Some(mcp_registry),
            external_tools,
        ))
    };

    ToolInvocation {
        requested: tool_request.clone(),
        effective,
        action,
    }
}

pub fn validate_tool_invocation(
    invocation: &ToolInvocation,
    mcp_registry: &McpRegistry,
    config: &RunConfig,
) -> Result<(), ToolExecutionFailure> {
    if let Some(definition) = &config.subagents.effective_definition
        && let Some(failure) = child_tool_policy_failure(
            &invocation.effective,
            Some(&definition.allowed_tools),
            Some("frozen custom agent policy"),
            mcp_registry,
            &config.external_tools,
        )
    {
        return Err(ToolExecutionFailure {
            request: invocation.effective.clone(),
            message: failure
                .error
                .clone()
                .unwrap_or_else(|| "custom agent tool is not allowed".into()),
        });
    }
    validate_shell_readiness(invocation, config)?;
    orca_tools::validate_with_mcp_and_external(
        &invocation.effective,
        Some(mcp_registry),
        &config.external_tools,
    )
    .map_err(|error| ToolExecutionFailure {
        request: invocation.effective.clone(),
        message: format!("tool arguments failed schema validation: {error}"),
    })
}

pub(crate) fn validate_shell_readiness(
    invocation: &ToolInvocation,
    config: &RunConfig,
) -> Result<(), ToolExecutionFailure> {
    let readiness = crate::shell_readiness::ShellReadiness::for_config(config);
    unavailable_shell_tool_failure(invocation, &readiness).map_or(Ok(()), Err)
}

fn unavailable_shell_tool_failure(
    invocation: &ToolInvocation,
    readiness: &crate::shell_readiness::ShellReadiness,
) -> Option<ToolExecutionFailure> {
    readiness
        .blocks_tool(&invocation.effective.name)
        .then(|| ToolExecutionFailure {
            request: invocation.effective.clone(),
            message: readiness
                .failure_message()
                .expect("blocked shell readiness has a failure message"),
        })
}

pub fn validate_tool_invocation_with_external(
    invocation: &ToolInvocation,
    mcp_registry: &McpRegistry,
    external_tools: &[ExternalToolConfig],
) -> Result<(), ToolExecutionFailure> {
    orca_tools::validate_with_mcp_and_external(
        &invocation.effective,
        Some(mcp_registry),
        external_tools,
    )
    .map_err(|error| ToolExecutionFailure {
        request: invocation.effective.clone(),
        message: format!("tool arguments failed schema validation: {error}"),
    })
}

pub fn apply_pre_tool_outcome(
    invocation: ToolInvocation,
    outcome: &HookOutcome,
    mcp_registry: &McpRegistry,
    config: &RunConfig,
) -> Result<ToolInvocation, ToolExecutionFailure> {
    let effective = tool_request_with_hook_outcome(&invocation.effective, outcome);
    let updated = ToolInvocation {
        effective,
        ..invocation
    };
    validate_tool_invocation(&updated, mcp_registry, config)?;
    Ok(updated)
}

pub fn apply_pre_tool_outcome_with_external(
    invocation: ToolInvocation,
    outcome: &HookOutcome,
    mcp_registry: &McpRegistry,
    external_tools: &[ExternalToolConfig],
) -> Result<ToolInvocation, ToolExecutionFailure> {
    let effective = tool_request_with_hook_outcome(&invocation.effective, outcome);
    let updated = ToolInvocation {
        effective,
        ..invocation
    };
    validate_tool_invocation_with_external(&updated, mcp_registry, external_tools)?;
    Ok(updated)
}

pub fn approval_request_for_invocation(invocation: &ToolInvocation) -> Option<ApprovalRequest> {
    let action = invocation.action?;
    Some(ApprovalRequest {
        id: format!("approval-{}", invocation.requested.id),
        action,
        description: format!(
            "{} requested {}",
            invocation.effective.name.as_str(),
            action.as_str()
        ),
        tool: Some(invocation.effective.name.as_str().to_string()),
        target: invocation.effective.target.clone(),
        preview: None,
    })
}

#[cfg(test)]
mod tests {
    use orca_core::approval_rules::PermissionRules;
    use orca_core::approval_types::{ActionKind, ApprovalMode};
    use orca_core::config::{
        HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, RunConfig, ThemeName,
        ToolConfig, WorkflowConfig,
    };
    use orca_core::external_config::ExternalToolConfig;
    use orca_core::mcp_types::McpTool;
    use orca_core::model::ModelSelection;
    use orca_core::provider_types::ProviderStep;
    use orca_core::subagent_config::SubagentConfig;
    use orca_core::subagent_types::SubagentType;
    use orca_core::tool_types::{ToolName, ToolRequest};
    use orca_mcp::McpRegistry;
    use serde_json::json;

    use crate::hooks::HookOutcome;

    use super::{
        AgentToolPolicyContext, ProviderToolDefinition, ToolInvocation, apply_pre_tool_outcome,
        approval_request_for_invocation, omit_unavailable_shell_launch_tools,
        prepare_tool_invocation, provider_config_for_agent_loop, provider_tool_schema_override,
        tool_requests_from_provider_steps, unavailable_shell_tool_failure,
        validate_tool_invocation,
    };

    fn config_with_external(external_tools: Vec<ExternalToolConfig>) -> RunConfig {
        RunConfig {
            prompt: "test".to_string(),
            app_version: "test".to_string(),
            cwd: Some(std::env::current_dir().expect("cwd")),
            provider: ProviderKind::Mock,
            model: ModelSelection::from_unchecked(Some("mock".to_string())),
            model_runtime: ModelRuntimeConfig::default(),
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            api_key: None,
            base_url: None,
            approval_mode: ApprovalMode::Suggest,
            execution_profile: orca_core::capability::ExecutionProfile::Workspace,
            output_format: OutputFormat::Jsonl,
            verifier: None,
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            active_permission_profile: None,
            permission_profiles: Default::default(),
            runtime_workspace_roots: None,
            theme: ThemeName::Dark,
            mcp_servers: Vec::new(),
            permission_rules: PermissionRules::default(),
            additional_working_directories: Vec::new(),
            budget: Default::default(),
            hooks: Vec::new(),
            workflows: WorkflowConfig::default(),
            subagents: SubagentConfig {
                max_depth: 1,
                ..SubagentConfig::default()
            },
            tools: ToolConfig {
                shell_enforcement_decision: Some(
                    orca_core::capability::SandboxEnforcementDecision::new(
                        orca_core::capability::EnforcementState::Enforced,
                        "test-sandbox",
                        Vec::new(),
                    ),
                ),
                ..ToolConfig::default()
            },
            external_tools,
            vim_mode: false,
            vim_insert_escape: None,
            update_check: false,
            desktop_notifications: false,
            terminal_notifications: false,
            auto_memory: false,
        }
    }

    fn request(
        name: ToolName,
        action: ActionKind,
        target: Option<&str>,
        raw: Option<&str>,
    ) -> ToolRequest {
        ToolRequest {
            id: "tool-1".to_string(),
            name,
            action,
            target: target.map(str::to_string),
            raw_arguments: raw.map(str::to_string),
        }
    }

    fn schema_names(tools: &[ProviderToolDefinition]) -> Vec<&str> {
        tools.iter().map(|tool| tool.name.as_str()).collect()
    }

    #[test]
    fn custom_agent_catalog_is_model_visible_and_policy_rejects_mutations() {
        let home = tempfile::tempdir().unwrap();
        let cwd = tempfile::tempdir().unwrap();
        let _home = crate::history::redirect_test_orca_home(home.path());
        std::fs::create_dir(home.path().join("agents")).unwrap();
        std::fs::write(home.path().join("agents/audit.md"),
            "---\nname: audit\ndescription: Review safely\ntools: [read_file]\n---\nPrivate agent instructions.\n").unwrap();
        let mut config = config_with_external(Vec::new());
        config.cwd = Some(cwd.path().to_path_buf());
        let mcp = McpRegistry::default();
        let provider = provider_config_for_agent_loop(
            &config,
            0,
            &SubagentType::General,
            AgentToolPolicyContext::unrestricted(),
            &mcp,
        );
        let definitions = provider.tools_override.unwrap();
        let subagent = definitions
            .iter()
            .find(|tool| tool.name == "subagent")
            .unwrap();
        assert!(subagent.description.contains("audit: Review safely"));
        assert!(!subagent.description.contains("Private agent instructions."));
        assert!(
            subagent.input_schema["properties"]["subagent_type"]["enum"]
                .as_array()
                .unwrap()
                .contains(&json!("audit"))
        );
        let catalog = crate::subagent::discover_agents(&config, cwd.path(), &mcp);
        config.subagents.effective_definition = Some(catalog.agents["audit"].clone());
        let write = request(
            ToolName::Bash,
            ActionKind::Shell,
            Some("echo mutation"),
            Some(r#"{"command":"echo mutation"}"#),
        );
        let invocation = prepare_tool_invocation(&write, 1, &mcp, &config);
        let failure = validate_tool_invocation(&invocation, &mcp, &config).unwrap_err();
        assert!(failure.message.contains("disallows tool 'bash'"));
        let read = request(
            ToolName::ReadFile,
            ActionKind::Read,
            Some("source.rs"),
            Some(r#"{"path":"source.rs"}"#),
        );
        let invocation = prepare_tool_invocation(&read, 1, &mcp, &config);
        assert!(validate_tool_invocation(&invocation, &mcp, &config).is_ok());
    }

    #[test]
    fn provider_tool_schema_override_exposes_root_agent_tools() {
        let registry = McpRegistry::default();
        let tools = provider_tool_schema_override(
            0,
            &SubagentType::General,
            AgentToolPolicyContext::unrestricted(),
            &registry,
            &[],
        )
        .expect("root tool schema");
        let names = schema_names(&tools);

        assert!(names.contains(&"subagent"));
        assert!(names.contains(&"bash"));
        assert!(!names.contains(&"get_goal"));
        assert!(!names.contains(&"create_goal"));
        assert!(!names.contains(&"update_goal"));
    }

    #[test]
    fn unavailable_shell_omits_only_new_process_tools_from_provider_catalog() {
        let registry = McpRegistry::default();
        let original = provider_tool_schema_override(
            0,
            &SubagentType::General,
            AgentToolPolicyContext::unrestricted(),
            &registry,
            &[],
        )
        .expect("root tool schema");
        let mut tools = original.clone();
        let readiness = crate::shell_readiness::ShellReadiness::Blocked {
            detail: "seatbelt probe terminated by signal 6".to_string(),
            remediation: "repair sandbox support".to_string(),
        };

        omit_unavailable_shell_launch_tools(&mut tools, &readiness);

        let names = schema_names(&tools);
        assert!(!names.contains(&"bash"));
        assert!(!names.contains(&"exec_command"));
        assert!(names.contains(&"write_stdin"));
        assert!(names.contains(&"edit"));
        assert!(names.contains(&"read_file"));

        let mut restored = original;
        omit_unavailable_shell_launch_tools(
            &mut restored,
            &crate::shell_readiness::ShellReadiness::Available,
        );
        let restored_names = schema_names(&restored);
        assert!(restored_names.contains(&"bash"));
        assert!(restored_names.contains(&"exec_command"));
    }

    #[test]
    fn unavailable_shell_rejects_a_stale_launch_request_before_dispatch() {
        let request = request(
            ToolName::ExecCommand,
            ActionKind::Shell,
            Some("cargo test"),
            Some(r#"{"cmd":"cargo test"}"#),
        );
        let invocation = ToolInvocation {
            requested: request.clone(),
            effective: request,
            action: Some(ActionKind::Shell),
        };
        let readiness = crate::shell_readiness::ShellReadiness::Blocked {
            detail: "seatbelt probe terminated by signal 6".to_string(),
            remediation: "repair sandbox support".to_string(),
        };

        let failure = unavailable_shell_tool_failure(&invocation, &readiness)
            .expect("stale shell invocation must fail");

        assert_eq!(failure.request.name, ToolName::ExecCommand);
        assert!(failure.message.contains("signal 6"));
    }

    #[test]
    fn provider_tool_schema_override_exposes_goal_tools_only_in_goal_mode() {
        let registry = McpRegistry::default();
        let tools = provider_tool_schema_override(
            0,
            &SubagentType::General,
            AgentToolPolicyContext::goal_mode(),
            &registry,
            &[],
        )
        .expect("goal tool schema");
        let names = schema_names(&tools);

        assert!(names.contains(&"get_goal"));
        assert!(names.contains(&"create_goal"));
        assert!(names.contains(&"update_goal"));
    }

    #[test]
    fn provider_tool_schema_override_limits_child_agent_to_allowed_tools() {
        let registry = McpRegistry::default();
        let allowed = vec!["read_file".to_string()];
        let tools = provider_tool_schema_override(
            1,
            &SubagentType::General,
            AgentToolPolicyContext::new(Some(&allowed), Some("test child")),
            &registry,
            &[],
        )
        .expect("child allowed tool schema");
        let names = schema_names(&tools);

        assert!(names.contains(&"read_file"));
        assert!(!names.contains(&"bash"));
        assert!(!names.contains(&"subagent"));
    }

    #[test]
    fn provider_tool_schema_override_limits_root_agent_to_allowed_tools() {
        let registry = McpRegistry::default();
        let allowed = vec!["read_file".to_string()];
        let tools = provider_tool_schema_override(
            0,
            &SubagentType::General,
            AgentToolPolicyContext::new(Some(&allowed), Some("runtime directive")),
            &registry,
            &[],
        )
        .expect("root allowed tool schema");
        let names = schema_names(&tools);

        assert!(names.contains(&"read_file"));
        assert!(!names.contains(&"bash"));
        assert!(!names.contains(&"subagent"));
    }

    #[test]
    fn agent_tool_policy_context_replaces_allowed_tools_for_runtime_directive() {
        let allowed = vec!["read_file".to_string()];
        let policy = AgentToolPolicyContext::unrestricted()
            .replace_allowed_tools(Some(&allowed), "runtime directive");

        assert_eq!(policy.allowed_tools(), Some(allowed.as_slice()));
        assert_eq!(policy.label(), Some("runtime directive"));
    }

    #[test]
    fn provider_tool_schema_override_uses_child_subagent_type_policy() {
        let registry = McpRegistry::default();
        let tools = provider_tool_schema_override(
            1,
            &SubagentType::CodeReviewer,
            AgentToolPolicyContext::unrestricted(),
            &registry,
            &[],
        )
        .expect("child typed tool schema");
        let names = schema_names(&tools);

        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"grep"));
        assert!(!names.contains(&"bash"));
        assert!(!names.contains(&"subagent"));
    }

    #[test]
    fn provider_config_for_agent_loop_builds_schema_limited_provider_config() {
        let registry = McpRegistry::default();
        let allowed = vec!["read_file".to_string()];
        let mut config = config_with_external(Vec::new());
        config.api_key = Some("test-key".to_string());
        config.base_url = Some("https://provider.test".to_string());

        let provider_config = provider_config_for_agent_loop(
            &config,
            1,
            &SubagentType::General,
            AgentToolPolicyContext::new(Some(&allowed), Some("test child")),
            &registry,
        );

        assert_eq!(provider_config.api_key.as_deref(), Some("test-key"));
        assert_eq!(
            provider_config.base_url.as_deref(),
            Some("https://provider.test")
        );
        assert_eq!(provider_config.model.as_deref(), Some("mock"));
        assert!(provider_config.mcp_registry.is_some());
        assert_eq!(provider_config.external_tools.len(), 0);

        let tools = provider_config.tools_override.expect("tool override");
        let names = schema_names(&tools);
        assert!(names.contains(&"read_file"));
        assert!(!names.contains(&"bash"));
        assert!(!names.contains(&"subagent"));
    }

    #[test]
    fn full_auto_provider_catalog_restores_new_shell_process_tools() {
        let registry = McpRegistry::default();
        let mut config = config_with_external(Vec::new());
        config.approval_mode = ApprovalMode::FullAuto;

        let provider = provider_config_for_agent_loop(
            &config,
            0,
            &SubagentType::General,
            AgentToolPolicyContext::unrestricted(),
            &registry,
        );
        let definitions = provider.tools_override.expect("tool override");
        let names = schema_names(&definitions);

        assert!(names.contains(&"bash"));
        assert!(names.contains(&"exec_command"));
        assert!(names.contains(&"write_stdin"));
    }

    #[test]
    fn tool_requests_from_provider_steps_extracts_tool_calls_in_order() {
        let first = request(ToolName::ReadFile, ActionKind::Read, Some("one.txt"), None);
        let second = ToolRequest {
            id: "tool-2".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Shell,
            target: Some("echo hi".to_string()),
            raw_arguments: None,
        };
        let steps = vec![
            ProviderStep::MessageDelta("hello".to_string()),
            ProviderStep::ToolCall(first.clone()),
            ProviderStep::ReasoningDelta("thinking".to_string()),
            ProviderStep::ToolCall(second.clone()),
            ProviderStep::Error(orca_core::provider_types::ProviderError::other("ignored")),
        ];

        let requests = tool_requests_from_provider_steps(&steps);

        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].id, first.id);
        assert_eq!(requests[1].id, second.id);
    }

    #[test]
    fn invocation_uses_registry_action_instead_of_caller_supplied_action() {
        let config = config_with_external(Vec::new());
        let registry = McpRegistry::default();
        let request = request(ToolName::Bash, ActionKind::Read, Some("echo hi"), None);

        let invocation = prepare_tool_invocation(&request, 0, &registry, &config);

        assert_eq!(invocation.action, Some(ActionKind::Shell));
        let approval = approval_request_for_invocation(&invocation).expect("approval");
        assert_eq!(approval.action, ActionKind::Shell);
        assert_eq!(approval.id, "approval-tool-1");
        assert_eq!(approval.description, "bash requested shell");
        assert_eq!(approval.tool, Some("bash".to_string()));
        assert_eq!(approval.target, Some("echo hi".to_string()));
        assert_eq!(approval.preview, None);
    }

    #[test]
    fn approval_names_the_effective_tool_after_rewrite() {
        let invocation = super::ToolInvocation {
            requested: request(ToolName::Bash, ActionKind::Shell, Some("echo hi"), None),
            effective: request(
                ToolName::ReadFile,
                ActionKind::Read,
                Some("notes.txt"),
                None,
            ),
            action: Some(ActionKind::Read),
        };

        let approval = approval_request_for_invocation(&invocation).expect("approval");

        assert_eq!(approval.id, "approval-tool-1");
        assert_eq!(approval.description, "read_file requested read");
        assert_eq!(approval.tool.as_deref(), Some("read_file"));
        assert_eq!(approval.target.as_deref(), Some("notes.txt"));
    }

    #[test]
    fn invocation_uses_external_tool_action_kind() {
        let config = config_with_external(vec![ExternalToolConfig {
            name: "deploy".to_string(),
            description: "deploy".to_string(),
            action_kind: ActionKind::Shell,
            command: "echo deploy".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "env": { "type": "string" }
                },
                "required": ["env"],
                "additionalProperties": false
            }),
        }]);
        let registry = McpRegistry::default();
        let request = request(
            ToolName::plain("deploy"),
            ActionKind::Read,
            Some("prod"),
            Some(r#"{"env":"prod"}"#),
        );

        let invocation = prepare_tool_invocation(&request, 0, &registry, &config);

        assert_eq!(invocation.action, Some(ActionKind::Shell));
    }

    #[test]
    fn invocation_uses_mcp_tool_action_kind() {
        let config = config_with_external(Vec::new());
        let registry = McpRegistry::from_tools_for_test(vec![McpTool {
            server: "local".to_string(),
            name: "write".to_string(),
            schema_name: "mcp__local__write".to_string(),
            description: Some("write via mcp".to_string()),
            input_schema: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }]);
        let request = request(
            ToolName::Mcp("mcp__local__write".to_string()),
            ActionKind::Read,
            None,
            Some("{}"),
        );

        let invocation = prepare_tool_invocation(&request, 0, &registry, &config);

        assert_eq!(invocation.action, Some(ActionKind::Write));
    }

    #[test]
    fn subagent_max_depth_blocks_approval_request() {
        let config = config_with_external(Vec::new());
        let registry = McpRegistry::default();
        let request = request(ToolName::Subagent, ActionKind::Agent, Some("audit"), None);

        let invocation = prepare_tool_invocation(&request, 1, &registry, &config);

        assert_eq!(invocation.action, None);
        assert!(approval_request_for_invocation(&invocation).is_none());
    }

    #[test]
    fn invalid_external_arguments_report_shared_validation_error() {
        let config = config_with_external(vec![ExternalToolConfig {
            name: "deploy".to_string(),
            description: "deploy".to_string(),
            action_kind: ActionKind::Shell,
            command: "echo deploy".to_string(),
            schema: json!({
                "type": "object",
                "properties": {
                    "env": { "type": "string" }
                },
                "required": ["env"],
                "additionalProperties": false
            }),
        }]);
        let registry = McpRegistry::default();
        let request = request(
            ToolName::plain("deploy"),
            ActionKind::Read,
            Some("prod"),
            Some(r#"{"unexpected":"prod"}"#),
        );
        let invocation = prepare_tool_invocation(&request, 0, &registry, &config);

        let failure =
            validate_tool_invocation(&invocation, &registry, &config).expect_err("invalid args");

        assert!(
            failure
                .message
                .contains("tool arguments failed schema validation")
        );
        assert!(
            failure
                .message
                .contains("missing required property \"env\"")
        );
    }

    #[test]
    fn hook_modified_target_keeps_shared_validation_path() {
        let mut config = config_with_external(Vec::new());
        config.approval_mode = ApprovalMode::FullAuto;
        let registry = McpRegistry::default();
        let request = request(
            ToolName::Bash,
            ActionKind::Shell,
            Some("echo before"),
            Some(r#"{"command":"echo before"}"#),
        );
        let invocation = prepare_tool_invocation(&request, 0, &registry, &config);
        let outcome = HookOutcome {
            modified_target: Some("echo after".to_string()),
            injected_context: Vec::new(),
        };

        let invocation = apply_pre_tool_outcome(invocation, &outcome, &registry, &config)
            .expect("hook-modified request remains valid");

        assert_eq!(invocation.effective.target.as_deref(), Some("echo after"));
        assert_eq!(invocation.action, Some(ActionKind::Shell));
    }
}
