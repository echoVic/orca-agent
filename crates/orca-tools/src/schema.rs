use std::collections::HashSet;

use orca_core::subagent_types::{SubagentType, builtin_agents, resolve_builtin_agent};
use orca_core::tool_types::{ToolName, ToolRequest};
use serde_json::Value;

use crate::registry::ToolRegistry;

/// Renders the selection contract for every built-in role. This is the only
/// place that turns the role catalog into model-visible prose, so the tool
/// description, the tool schema enum, and the runtime ceiling cannot drift.
///
/// `capsule` keeps the main system prompt short by emitting one line per role;
/// the full selection guidance is reserved for the `subagent` tool description.
pub fn render_builtin_role_catalog(capsule: bool) -> String {
    let mut output = String::new();
    for descriptor in builtin_agents() {
        if capsule {
            output.push_str(&format!(
                "\n- `{}`: {}",
                descriptor.name, descriptor.when_to_use
            ));
            continue;
        }
        output.push_str(&format!(
            "\n- `{}`: {}",
            descriptor.name, descriptor.when_to_use
        ));
        output.push_str(&format!("\n  Avoid when: {}", descriptor.avoid_when));
        output.push_str(&format!("\n  Tools: {}", descriptor.tools.join(", ")));
        output.push_str(&format!(
            "\n  Report: {}",
            descriptor.deliverables.join("; ")
        ));
    }
    output
}

/// The canonical built-in role names, in catalog order.
pub fn builtin_role_names() -> Vec<&'static str> {
    builtin_agents().iter().map(|d| d.name).collect()
}

/// Advertise only resolved definitions; bodies and filesystem paths stay out of the catalog.
pub fn apply_subagent_catalog(
    description: &mut String,
    input_schema: &mut Value,
    catalog: &orca_core::subagent_types::agent_definition::AgentCatalog,
) {
    let mut names = builtin_role_names();
    names.extend(catalog.agents.keys().map(String::as_str));
    input_schema["properties"]["subagent_type"]["enum"] = serde_json::json!(names);
    description.push_str("\n\nBuilt-in roles (use the identifier verbatim):");
    description.push_str(&render_builtin_role_catalog(false));
    if !catalog.agents.is_empty() {
        description.push_str("\n\nAvailable custom agents (identifier: description):");
        for agent in catalog.agents.values() {
            description.push_str(&format!("\n- {}: {}", agent.name, agent.description));
        }
    }
    if !catalog.diagnostics.is_empty() {
        description
            .push_str("\n\nSome agent definitions were invalid or ambiguous and were excluded.");
    }
    description.push_str(
        "\n\nAccepted aliases: explore/scout=explorer, reviewer=code_reviewer, tester=test_writer, debug=debugger, docs=documenter.",
    );
}

/// Resolves a requested identifier to the canonical built-in role name.
///
/// Built-in aliases resolve to their canonical role; anything else is a custom
/// agent identifier and returns `None`.
pub fn canonical_builtin_role(requested: &str) -> Option<&'static str> {
    resolve_builtin_agent(requested).map(|descriptor| descriptor.name)
}

const GOAL_TOOL_NAMES: &[&str] = &["get_goal", "create_goal", "update_goal"];
const STRICT_MODE_TOOL_NAMES: &[&str] = &["glob", "update_goal", "update_plan"];

#[derive(Clone, Debug, PartialEq)]
pub struct CanonicalToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub strict_capable: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolPolicy {
    selection: ToolSelection,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ToolSelection {
    Base,
    Goal,
    Allowed(Vec<String>),
    Subagent(Vec<String>),
}

impl ToolPolicy {
    pub fn base() -> Self {
        Self {
            selection: ToolSelection::Base,
        }
    }

    pub fn goal() -> Self {
        Self {
            selection: ToolSelection::Goal,
        }
    }

    pub fn allowed<S: AsRef<str>>(names: &[S]) -> Self {
        Self {
            selection: ToolSelection::Allowed(
                names.iter().map(|name| name.as_ref().to_string()).collect(),
            ),
        }
    }

    pub fn for_subagent(subagent_type: &SubagentType) -> Self {
        Self {
            selection: ToolSelection::Subagent(
                subagent_type
                    .allowed_tools()
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
            ),
        }
    }
}

pub fn canonical_tool_definitions(
    policy: &ToolPolicy,
    registry: &ToolRegistry,
) -> Vec<CanonicalToolDefinition> {
    let allowed = match &policy.selection {
        ToolSelection::Allowed(names) | ToolSelection::Subagent(names) => {
            Some(canonical_allowed_names(registry, names))
        }
        ToolSelection::Base | ToolSelection::Goal => None,
    };

    registry
        .model_visible_tools()
        .filter(|tool| match &policy.selection {
            ToolSelection::Base => !GOAL_TOOL_NAMES.contains(&tool.name()),
            ToolSelection::Goal => true,
            ToolSelection::Allowed(_) => allowed
                .as_ref()
                .is_some_and(|allowed| allowed.contains(tool.name())),
            ToolSelection::Subagent(_) => {
                tool.name() != "subagent"
                    && allowed
                        .as_ref()
                        .is_some_and(|allowed| allowed.contains(tool.name()))
            }
        })
        .map(|tool| CanonicalToolDefinition {
            name: tool.name().to_string(),
            description: tool.description().to_string(),
            input_schema: tool.spec().input_schema.clone(),
            strict_capable: STRICT_MODE_TOOL_NAMES.contains(&tool.name()),
        })
        .collect()
}

pub fn normalize_tool_arguments(name: &ToolName, value: Value) -> Result<Value, String> {
    let raw = serde_json::to_string(&value)
        .map_err(|error| format!("failed to serialize tool arguments: {error}"))?;
    let normalized = match name {
        ToolName::UpdatePlan => crate::update_plan::normalize_raw_arguments(&raw).unwrap_or(raw),
        ToolName::UpdateGoal => crate::update_goal::normalized_update_raw_arguments(&raw),
        _ => raw,
    };
    serde_json::from_str(&normalized)
        .map_err(|error| format!("failed to parse normalized tool arguments: {error}"))
}

pub fn normalize_tool_request(
    registry: &ToolRegistry,
    request: &ToolRequest,
) -> Result<ToolRequest, String> {
    let resolved = registry.resolve(request.name.as_str());
    let name = resolved
        .as_ref()
        .map(|resolved| resolved.requested_name.clone())
        .unwrap_or_else(|| request.name.clone());
    let action = resolved
        .as_ref()
        .map(|resolved| resolved.spec.capabilities.action_kind())
        .unwrap_or(request.action);

    let (raw_arguments, arguments) = match request.raw_arguments.as_deref() {
        Some(raw) => {
            let value: Value = serde_json::from_str(raw)
                .map_err(|error| format!("arguments are not valid JSON: {error}"))?;
            let normalized = normalize_tool_arguments(&name, value.clone())?;
            let raw_arguments = if normalized == value {
                raw.to_string()
            } else {
                serde_json::to_string(&normalized)
                    .map_err(|error| format!("failed to serialize normalized arguments: {error}"))?
            };
            (Some(raw_arguments), Some(normalized))
        }
        None => (None, None),
    };
    let target = arguments
        .as_ref()
        .and_then(|arguments| tool_target(&name, arguments))
        .or_else(|| request.target.clone());

    Ok(ToolRequest {
        id: request.id.clone(),
        name,
        action,
        target,
        raw_arguments,
    })
}

fn canonical_allowed_names(registry: &ToolRegistry, names: &[String]) -> HashSet<String> {
    names
        .iter()
        .filter_map(|name| {
            registry
                .resolve(name)
                .map(|resolved| resolved.tool.name().to_string())
        })
        .collect()
}

fn tool_target(name: &ToolName, arguments: &Value) -> Option<String> {
    match name {
        ToolName::ReadFile | ToolName::Edit | ToolName::WriteFile => {
            arguments["path"].as_str().map(String::from)
        }
        ToolName::ListFiles | ToolName::Glob => arguments["path"]
            .as_str()
            .map(String::from)
            .or_else(|| Some(".".to_string())),
        ToolName::Grep => arguments["pattern"].as_str().map(String::from),
        ToolName::Bash => arguments["command"].as_str().map(String::from),
        ToolName::TaskReadOutput | ToolName::TaskSendInput | ToolName::TaskWait => {
            arguments["task_id"].as_str().map(String::from).or_else(|| {
                arguments["task_ids"]
                    .as_array()
                    .map(|ids| format!("{} tasks", ids.len()))
            })
        }
        ToolName::GitStatus => Some(".".to_string()),
        ToolName::Subagent => arguments["description"]
            .as_str()
            .or_else(|| arguments["prompt"].as_str())
            .map(String::from),
        ToolName::WebSearch => arguments["query"].as_str().map(String::from),
        ToolName::UpdatePlan => arguments["plan"]
            .as_array()
            .map(|plan| format!("{} items", plan.len())),
        ToolName::Mcp(name) | ToolName::External(name) => Some(name.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn external_tool(name: &str) -> orca_core::external_config::ExternalToolConfig {
        orca_core::external_config::ExternalToolConfig {
            name: name.to_string(),
            description: name.to_string(),
            action_kind: orca_core::approval_types::ActionKind::Read,
            command: "true".to_string(),
            schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        }
    }

    #[test]
    fn subagent_policy_does_not_grant_unlisted_external_tools() {
        let external_tools = vec![external_tool("private_lookup")];
        let registry = crate::registry::tool_registry_with_mcp_and_external(None, &external_tools);
        let policy = ToolPolicy {
            selection: ToolSelection::Subagent(vec!["read_file".to_string()]),
        };

        let names = canonical_tool_definitions(&policy, &registry)
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>();

        assert!(names.iter().any(|name| name == "read_file"));
        assert!(!names.iter().any(|name| name == "private_lookup"));
    }

    #[test]
    fn normalizes_plan_boolean_status_flags() {
        let normalized = normalize_tool_arguments(
            &ToolName::UpdatePlan,
            serde_json::json!({
                "plan": [{ "step": "inspect", "completed": true }]
            }),
        )
        .expect("normalize plan");

        assert_eq!(normalized["plan"][0]["status"], "completed");
        assert!(normalized["plan"][0].get("completed").is_none());
    }

    #[test]
    fn command_and_task_tools_extract_a_stable_target() {
        // Every task tool addresses work by task_id, so the audit target is the
        // task identity rather than a second session id.
        assert_eq!(
            tool_target(
                &ToolName::Bash,
                &serde_json::json!({"command": "vim README.md"}),
            ),
            Some("vim README.md".to_string())
        );
        assert_eq!(
            tool_target(
                &ToolName::TaskSendInput,
                &serde_json::json!({"task_id": "cmd_1", "chars": "x"}),
            ),
            Some("cmd_1".to_string())
        );
        assert_eq!(
            tool_target(
                &ToolName::TaskWait,
                &serde_json::json!({"task_ids": ["cmd_1", "cmd_2"]}),
            ),
            Some("2 tasks".to_string())
        );
    }
}
