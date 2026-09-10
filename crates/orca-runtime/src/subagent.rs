use std::path::Path;

use orca_core::config::{DelegationSnapshot, RunConfig};
use orca_core::subagent_config::FrozenAgentConfig;
use orca_core::subagent_types::SubagentType;
use orca_core::subagent_types::agent_definition::{AgentCatalog, validate_name};
use orca_core::tool_types::ToolRequest;
use orca_mcp::McpRegistry;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SubagentRequest {
    pub description: String,
    pub prompt: String,
    pub subagent_type: SubagentType,
    pub model: Option<String>,
    pub mode: SubagentMode,
    pub isolation: SubagentIsolation,
    pub schema: Option<Value>,
    #[serde(default)]
    pub resume_from: Option<String>,
    #[serde(default)]
    pub delegation: Option<DelegationSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frozen_agent: Option<FrozenAgentConfig>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentMode {
    Sync,
    Async,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentIsolation {
    None,
    Worktree,
}

pub fn extract_subagent_field(tool_request: &ToolRequest, field: &str) -> Option<String> {
    let raw = tool_request.raw_arguments.as_ref()?;
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    value[field].as_str().map(String::from)
}

pub fn extract_subagent_json_field(tool_request: &ToolRequest, field: &str) -> Option<Value> {
    let raw = tool_request.raw_arguments.as_ref()?;
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    value.get(field).cloned()
}

pub fn create_subagent_request(tool_request: &ToolRequest) -> SubagentRequest {
    let description = extract_subagent_field(tool_request, "description")
        .or_else(|| tool_request.target.clone())
        .unwrap_or_else(|| "subagent".to_string());

    let prompt =
        extract_subagent_field(tool_request, "prompt").unwrap_or_else(|| description.clone());

    let subagent_type = extract_subagent_field(tool_request, "subagent_type")
        .map(|s| SubagentType::from_str(&s))
        .unwrap_or_default();
    let model = extract_subagent_field(tool_request, "model")
        .filter(|model| orca_core::model::validate_model(model).is_ok());
    let mode = match extract_subagent_field(tool_request, "mode").as_deref() {
        Some("async") => SubagentMode::Async,
        _ => SubagentMode::Sync,
    };
    let isolation = match extract_subagent_field(tool_request, "isolation").as_deref() {
        Some("worktree") => SubagentIsolation::Worktree,
        _ => SubagentIsolation::None,
    };
    let schema = extract_subagent_json_field(tool_request, "schema");
    let resume_from = extract_subagent_field(tool_request, "resume_from")
        .map(|selector| selector.trim().to_string())
        .filter(|selector| !selector.is_empty());

    SubagentRequest {
        description,
        prompt,
        subagent_type,
        model,
        mode,
        isolation,
        schema,
        resume_from,
        delegation: None,
        frozen_agent: None,
    }
}

pub fn with_delegation_snapshot(
    mut request: SubagentRequest,
    snapshot: DelegationSnapshot,
) -> SubagentRequest {
    request.delegation = Some(snapshot);
    request
}

pub(crate) fn discover_agents(config: &RunConfig, cwd: &Path, mcp: &McpRegistry) -> AgentCatalog {
    let registry = orca_tools::registry::tool_registry_with_mcp_and_external(
        Some(mcp),
        &config.external_tools,
    );
    let tools = registry
        .model_visible_tools()
        .flat_map(|tool| {
            std::iter::once(tool.name().to_string()).chain(
                tool.spec()
                    .aliases
                    .iter()
                    .map(|alias| alias.as_str().to_string()),
            )
        })
        .collect::<Vec<_>>();
    let models = config.model.as_option().into_iter().collect::<Vec<_>>();
    AgentCatalog::discover(cwd, &tools, &models)
}

/// Called on the admitting thread, before detached/threaded execution can observe changed files.
pub(crate) fn freeze_agent_request(
    config: &RunConfig,
    cwd: &Path,
    mcp: &McpRegistry,
    request: &mut SubagentRequest,
) -> Result<(), String> {
    let SubagentType::Custom(name) = &request.subagent_type else {
        return Ok(());
    };
    if request.resume_from.is_some() {
        return Ok(()); // Resume obtains its snapshot exclusively from the continuation.
    }
    validate_name(name)?;
    let catalog = discover_agents(config, cwd, mcp);
    let mut definition = catalog.agents.get(name).cloned().ok_or_else(|| {
        let diagnostics = catalog
            .diagnostics
            .iter()
            .map(|entry| format!("{}: {}", entry.path.display(), entry.message))
            .collect::<Vec<_>>()
            .join("; ");
        format!(
            "unknown or invalid custom agent '{name}'{suffix}",
            suffix = if diagnostics.is_empty() {
                String::new()
            } else {
                format!(": {diagnostics}")
            }
        )
    })?;
    if let Some(parent) = &config.subagents.effective_definition {
        definition.narrow_tools(&parent.allowed_tools);
    }
    if let Some(ceiling) = &config.subagents.inherited_tools {
        definition.narrow_tools(&canonical_tool_ceiling(ceiling, config, mcp));
    }
    let mut delegation = DelegationSnapshot::from_config(config);
    // Preserve the actual execution profile, including caller-imposed restrictions.
    delegation.execution_profile = config.execution_profile;
    request.model = request.model.clone().or_else(|| definition.model.clone());
    definition.model = config
        .model
        .with_subagent_override(request.model.clone())
        .as_option();
    request.model = definition.model.clone();
    request.delegation = Some(delegation.clone());
    request.frozen_agent = Some(FrozenAgentConfig {
        definition,
        delegation,
    });
    Ok(())
}

/// Applies only immutable launch material; this function never performs discovery.
pub(crate) fn apply_frozen_agent(
    config: &mut RunConfig,
    subagent_type: &SubagentType,
    frozen: Option<&FrozenAgentConfig>,
) -> Result<(), String> {
    match (subagent_type, frozen) {
        (SubagentType::Custom(name), Some(frozen)) if name == &frozen.definition.name => {
            frozen.definition.validate()?;
            frozen
                .delegation
                .apply_to(config, frozen.definition.model.clone());
            config.subagents.effective_definition = Some(frozen.definition.clone());
            Ok(())
        }
        (SubagentType::Custom(_), _) => {
            Err("custom agent requires a matching frozen definition".into())
        }
        (_, Some(_)) => Err("built-in agent cannot carry a custom definition".into()),
        (_, None) => {
            config.subagents.effective_definition = None;
            Ok(())
        }
    }
}

pub(crate) fn restore_frozen_agent(
    config: &RunConfig,
    source: &crate::agent_continuation::PreparedContinuation,
) -> Result<Option<FrozenAgentConfig>, String> {
    let frozen = source.compatibility.frozen_agent.clone();
    if let Some(frozen) = &frozen {
        let mut current = DelegationSnapshot::from_config(config);
        current.execution_profile = config.execution_profile;
        if current != frozen.delegation {
            return Err(
                "continuation_incompatible: custom agent parent permissions or model changed"
                    .into(),
            );
        }
        if config
            .subagents
            .inherited_tools
            .as_ref()
            .is_some_and(|ceiling| {
                let ceiling = canonical_tool_ceiling(ceiling, config, &McpRegistry::default());
                frozen
                    .definition
                    .allowed_tools
                    .iter()
                    .any(|tool| !ceiling.contains(tool))
            })
        {
            return Err(
                "continuation_incompatible: custom agent parent tool policy narrowed".into(),
            );
        }
    } else if matches!(
        SubagentType::from_str(&source.compatibility.subagent_type),
        SubagentType::Custom(_)
    ) {
        return Err("continuation_incompatible: custom agent has no frozen definition".into());
    }
    Ok(frozen)
}

fn canonical_tool_ceiling(
    ceiling: &[String],
    config: &RunConfig,
    mcp: &McpRegistry,
) -> Vec<String> {
    let registry = orca_tools::registry::tool_registry_with_mcp_and_external(
        Some(mcp),
        &config.external_tools,
    );
    ceiling
        .iter()
        .filter_map(|tool| {
            registry
                .resolve(tool)
                .map(|resolved| resolved.tool.name().to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_types::ActionKind;
    use orca_core::tool_types::ToolName;

    #[test]
    fn create_request_with_all_fields() {
        let req = ToolRequest {
            id: "t1".to_string(),
            name: ToolName::Subagent,
            action: ActionKind::Read,
            target: Some("test task".to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "description": "review code",
                    "prompt": "review src/main.rs for bugs",
                    "subagent_type": "code_reviewer",
                    "model": "deepseek-v4-pro",
                    "isolation": "worktree",
                    "schema": { "type": "string" }
                })
                .to_string(),
            ),
        };
        let result = create_subagent_request(&req);
        assert_eq!(result.description, "review code");
        assert_eq!(result.prompt, "review src/main.rs for bugs");
        assert_eq!(result.subagent_type, SubagentType::CodeReviewer);
        assert_eq!(result.model.as_deref(), Some("deepseek-v4-pro"));
        assert_eq!(result.mode, SubagentMode::Sync);
        assert_eq!(result.isolation, SubagentIsolation::Worktree);
        assert_eq!(result.schema, Some(serde_json::json!({ "type": "string" })));
    }

    #[test]
    fn create_request_parses_async_mode() {
        let req = ToolRequest {
            id: "t4".to_string(),
            name: ToolName::Subagent,
            action: ActionKind::Read,
            target: Some("async task".to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "description": "async task",
                    "prompt": "inspect later",
                    "mode": "async"
                })
                .to_string(),
            ),
        };
        let result = create_subagent_request(&req);
        assert_eq!(result.mode, SubagentMode::Async);
    }

    #[test]
    fn create_request_defaults_to_general() {
        let req = ToolRequest {
            id: "t2".to_string(),
            name: ToolName::Subagent,
            action: ActionKind::Read,
            target: Some("analyze".to_string()),
            raw_arguments: Some(
                serde_json::json!({
                    "description": "analyze repo",
                    "prompt": "analyze the repository structure"
                })
                .to_string(),
            ),
        };
        let result = create_subagent_request(&req);
        assert_eq!(result.subagent_type, SubagentType::General);
    }

    #[test]
    fn create_request_falls_back_to_target() {
        let req = ToolRequest {
            id: "t3".to_string(),
            name: ToolName::Subagent,
            action: ActionKind::Read,
            target: Some("fallback desc".to_string()),
            raw_arguments: Some("{}".to_string()),
        };
        let result = create_subagent_request(&req);
        assert_eq!(result.description, "fallback desc");
        assert_eq!(result.prompt, "fallback desc");
    }
}
