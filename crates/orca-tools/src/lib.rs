#![deny(deprecated)]

use std::path::{Path, PathBuf};

use orca_core::approval_types::ActionKind;
use orca_core::external_config::ExternalToolConfig;
use orca_core::tool_types::{ToolName, ToolOutputTruncation, ToolRequest, ToolResult};
use orca_mcp::{McpElicitationHandler, McpRegistry};

pub mod bash;
pub mod edit;
pub mod external;
pub mod file_admission;
pub mod git;
pub mod glob;
pub mod grep;
pub mod list_files;
pub mod process;
pub mod read_file;
pub mod registry;
pub mod sandbox;
pub mod schema;
pub mod skills;
pub mod update_goal;
pub mod update_plan;
pub mod web_search;
pub mod write_file;

pub use registry::{Tool, ToolContext, ToolRegistry, validate_tool_request};

pub fn execute_with_mcp(
    request: &ToolRequest,
    cwd: &Path,
    mcp_registry: &McpRegistry,
) -> ToolResult {
    execute_with_mcp_and_external(request, cwd, mcp_registry, &[], 120)
}

pub fn execute_with_mcp_and_external(
    request: &ToolRequest,
    cwd: &Path,
    mcp_registry: &McpRegistry,
    external_tools: &[ExternalToolConfig],
    shell_timeout_secs: u64,
) -> ToolResult {
    execute_with_mcp_external_and_policy(
        request,
        cwd,
        mcp_registry,
        external_tools,
        ToolOutputTruncation::default(),
        shell_timeout_secs,
    )
}

pub fn execute_with_mcp_external_and_policy(
    request: &ToolRequest,
    cwd: &Path,
    mcp_registry: &McpRegistry,
    external_tools: &[ExternalToolConfig],
    output_truncation: ToolOutputTruncation,
    shell_timeout_secs: u64,
) -> ToolResult {
    execute_with_mcp_external_policy_or_cancel(
        request,
        cwd,
        mcp_registry,
        external_tools,
        output_truncation,
        shell_timeout_secs,
        || false,
    )
}

pub fn execute_with_mcp_external_policy_or_cancel(
    request: &ToolRequest,
    cwd: &Path,
    mcp_registry: &McpRegistry,
    external_tools: &[ExternalToolConfig],
    output_truncation: ToolOutputTruncation,
    shell_timeout_secs: u64,
    should_cancel: impl Fn() -> bool,
) -> ToolResult {
    execute_with_mcp_external_roots_policy_or_cancel(
        request,
        cwd,
        &[],
        mcp_registry,
        external_tools,
        output_truncation,
        shell_timeout_secs,
        should_cancel,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn execute_with_mcp_external_roots_policy_or_cancel(
    request: &ToolRequest,
    cwd: &Path,
    additional_roots: &[PathBuf],
    mcp_registry: &McpRegistry,
    external_tools: &[ExternalToolConfig],
    output_truncation: ToolOutputTruncation,
    shell_timeout_secs: u64,
    should_cancel: impl Fn() -> bool,
) -> ToolResult {
    execute_with_mcp_external_roots_policy_or_cancel_and_elicitation(
        request,
        cwd,
        additional_roots,
        mcp_registry,
        external_tools,
        output_truncation,
        shell_timeout_secs,
        None,
        should_cancel,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn execute_with_mcp_external_roots_policy_or_cancel_and_elicitation(
    request: &ToolRequest,
    cwd: &Path,
    additional_roots: &[PathBuf],
    mcp_registry: &McpRegistry,
    external_tools: &[ExternalToolConfig],
    output_truncation: ToolOutputTruncation,
    shell_timeout_secs: u64,
    mcp_elicitation_handler: Option<&dyn McpElicitationHandler>,
    should_cancel: impl Fn() -> bool,
) -> ToolResult {
    let shell_timeout = std::time::Duration::from_secs(shell_timeout_secs.max(1));
    let should_cancel = &should_cancel as &dyn Fn() -> bool;
    if !tool_uses_mcp_registry(&request.name) {
        if external_tools.is_empty() {
            let reg = registry::default_tool_registry();
            let ctx = registry::ToolContext::new(cwd)
                .with_output_truncation(output_truncation)
                .with_shell_timeout(shell_timeout)
                .with_additional_working_directories(additional_roots.iter().cloned())
                .with_cancel(should_cancel);
            return reg.execute(request, &ctx);
        }
        let reg = registry::tool_registry_with_mcp_and_external(None, external_tools);
        let ctx = registry::ToolContext::new(cwd)
            .with_output_truncation(output_truncation)
            .with_shell_timeout(shell_timeout)
            .with_additional_working_directories(additional_roots.iter().cloned())
            .with_cancel(should_cancel);
        return reg.execute(request, &ctx);
    }

    let reg = registry::tool_registry_with_mcp_and_external(Some(mcp_registry), external_tools);
    let mut ctx = registry::ToolContext::new(cwd)
        .with_output_truncation(output_truncation)
        .with_shell_timeout(shell_timeout)
        .with_additional_working_directories(additional_roots.iter().cloned())
        .with_mcp(mcp_registry)
        .with_cancel(should_cancel);
    if let Some(handler) = mcp_elicitation_handler {
        ctx = ctx.with_mcp_elicitation_handler(handler);
    }
    reg.execute(request, &ctx)
}

fn tool_uses_mcp_registry(name: &ToolName) -> bool {
    matches!(
        name,
        ToolName::Mcp(_)
            | ToolName::ListMcpResources
            | ToolName::ListMcpResourceTemplates
            | ToolName::ReadMcpResource
    )
}

pub fn validate_with_mcp_and_external(
    request: &ToolRequest,
    mcp_registry: Option<&McpRegistry>,
    external_tools: &[ExternalToolConfig],
) -> Result<(), String> {
    let reg = registry::tool_registry_with_mcp_and_external(mcp_registry, external_tools);
    registry::validate_tool_request(&reg, request)
}

pub fn tool_is_available_readonly_concurrent(request: &ToolRequest) -> bool {
    if runtime_owned_tool_requires_controller(&request.name) {
        return false;
    }
    let reg = registry::default_tool_registry();
    reg.resolve(request.name.as_str())
        .map(|resolved| {
            resolved.spec.capabilities.is_read_only()
                && resolved.spec.concurrent_safe
                && resolved.tool.is_concurrent_safe(request)
        })
        .unwrap_or(false)
}

fn runtime_owned_tool_requires_controller(name: &ToolName) -> bool {
    matches!(
        name,
        ToolName::SubagentStatus
            | ToolName::TaskList
            | ToolName::WorkflowReadMessages
            | ToolName::WorkflowListTasks
    )
}

pub fn canonical_action_kind(request: &ToolRequest) -> ActionKind {
    canonical_action_kind_with_mcp_and_external(request, None, &[])
}

pub fn canonical_action_kind_with_mcp_and_external(
    request: &ToolRequest,
    mcp_registry: Option<&McpRegistry>,
    external_tools: &[ExternalToolConfig],
) -> ActionKind {
    let reg = registry::tool_registry_with_mcp_and_external(mcp_registry, external_tools);
    reg.resolve(request.name.as_str())
        .map(|resolved| resolved.spec.capabilities.action_kind())
        .unwrap_or(request.action)
}

pub fn should_run_readonly_batch(max_read_parallel: usize, tool_request: &ToolRequest) -> bool {
    tool_is_available_readonly_concurrent(tool_request) && max_read_parallel > 1
}

pub fn collect_readonly_batch(
    max_read_parallel: usize,
    tool_requests: &[ToolRequest],
    start: usize,
) -> usize {
    let max_end = (start + max_read_parallel).min(tool_requests.len());
    let mut end = start;
    while end < max_end && tool_is_available_readonly_concurrent(&tool_requests[end]) {
        end += 1;
    }
    end
}

pub fn resolve_workspace_path(cwd: &Path, target: Option<&str>) -> Result<PathBuf, String> {
    let target = target.unwrap_or(".");
    let candidate = PathBuf::from(target);
    let joined = if candidate.is_absolute() {
        candidate
    } else {
        cwd.join(candidate)
    };

    let mut normalized = PathBuf::new();
    for component in joined.components() {
        match component {
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            std::path::Component::CurDir => {}
            _ => normalized.push(component),
        }
    }

    if !normalized.starts_with(cwd) {
        return Err(format!("path escapes workspace: {target}"));
    }

    if normalized.exists() {
        let canonical = normalized
            .canonicalize()
            .map_err(|e| format!("cannot resolve path: {e}"))?;
        let canonical_cwd = cwd
            .canonicalize()
            .map_err(|e| format!("cannot resolve cwd: {e}"))?;
        if !canonical.starts_with(&canonical_cwd) {
            return Err(format!("path escapes workspace via symlink: {target}"));
        }
    }

    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::approval_types::ActionKind;
    use orca_core::mcp_types::{McpServerConfig, McpTransportKind};
    use orca_core::tool_types::{
        InterruptSemantics, ReplaySemantics, ToolControlSemantics, ToolStatus, truncate_output,
    };
    use std::collections::HashMap;
    use std::fs;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    #[test]
    fn micro_compact_preserves_head_and_tail() {
        let output = format!("{}{}{}", "a".repeat(80), "middle", "z".repeat(80));
        let (truncated, was_truncated) = truncate_output(output, 80);
        assert!(was_truncated);
        assert!(truncated.starts_with("aaaa"));
        assert!(truncated.contains("micro-compacted"));
        assert!(truncated.ends_with("zzzz"));
    }

    #[test]
    fn default_registry_exposes_builtin_tool_metadata() {
        let reg = registry::default_tool_registry();

        let tool = reg
            .get("read_file")
            .expect("read_file is registered as a tool");
        let request = ToolRequest {
            id: "read".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("README.md".to_string()),
            raw_arguments: None,
        };

        assert_eq!(tool.name(), "read_file");
        assert_eq!(tool.action_kind(), ActionKind::Read);
        assert!(tool.is_read_only(&request));
        assert!(tool.is_concurrent_safe(&request));
        assert_eq!(reg.iter().next().map(|tool| tool.name()), Some("read_file"));

        assert_eq!(
            reg.get("web_search").unwrap().action_kind(),
            ActionKind::Network
        );
        assert_eq!(
            reg.get("subagent").unwrap().action_kind(),
            ActionKind::Agent
        );
    }

    #[test]
    fn registry_control_semantics_resolves_registered_identity_and_aliases() {
        let default_registry = registry::default_tool_registry();
        assert_eq!(
            default_registry.control_semantics(&ToolName::Bash),
            Some(ToolControlSemantics {
                interrupt: InterruptSemantics::CooperativeCancel,
                replay: ReplaySemantics::IndeterminateAfterStart,
            })
        );
        assert_eq!(
            default_registry.control_semantics(&ToolName::plain("list_files")),
            Some(ToolControlSemantics {
                interrupt: InterruptSemantics::WaitForTerminal,
                replay: ReplaySemantics::SafeToRetry,
            })
        );
        assert_eq!(
            default_registry.control_semantics(&ToolName::plain("not_registered")),
            None
        );

        let external_tools = vec![ExternalToolConfig {
            name: "inspect_remote".to_string(),
            description: "external read-like tool".to_string(),
            action_kind: ActionKind::Read,
            command: "printf inspected".to_string(),
            schema: serde_json::json!({}),
        }];
        let registry = registry::tool_registry_with_mcp_and_external(None, &external_tools);
        assert_eq!(
            registry.control_semantics(&ToolName::plain("inspect_remote")),
            Some(ToolControlSemantics {
                interrupt: InterruptSemantics::CooperativeCancel,
                replay: ReplaySemantics::IndeterminateAfterStart,
            })
        );
        assert_eq!(
            registry.control_semantics(&ToolName::ReadFile),
            Some(ToolControlSemantics {
                interrupt: InterruptSemantics::WaitForTerminal,
                replay: ReplaySemantics::SafeToRetry,
            }),
            "dynamic tools cannot shadow a builtin policy"
        );
    }

    #[test]
    fn registry_resolves_list_files_to_discovery_capabilities() {
        let reg = registry::default_tool_registry();
        let resolved = reg.resolve("list_files").expect("list_files alias");

        assert_eq!(resolved.tool.name(), "glob");
        assert!(resolved.spec.capabilities.is_read_only());
        assert_eq!(resolved.requested_name.as_str(), "list_files");
    }

    #[test]
    fn model_visible_tools_hide_list_files_after_glob_exists() {
        let reg = registry::default_tool_registry();
        let names = reg
            .model_visible_tools()
            .map(|tool| tool.name().to_string())
            .collect::<Vec<_>>();

        assert!(names.contains(&"glob".to_string()));
        assert!(!names.contains(&"list_files".to_string()));
    }

    #[test]
    fn request_user_input_is_not_registered_after_canonical_name_migration() {
        let reg = registry::default_tool_registry();
        assert!(reg.get("request_user_input").is_none());
        assert!(reg.resolve("request_user_input").is_none());
        assert!(
            reg.model_visible_tools()
                .all(|tool| tool.name() != "request_user_input")
        );
    }

    #[test]
    fn ask_user_question_tool_exposes_structured_questionnaire_contract() {
        let reg = registry::default_tool_registry();
        let tool = reg
            .get("ask_user_question")
            .expect("ask_user_question is registered");
        let schema = &tool.spec().input_schema;
        let question_items = &schema["properties"]["questions"];
        let option_items = &question_items["items"]["properties"]["options"];

        assert!(tool.spec().exposure.is_model_visible());
        assert_eq!(tool.spec().name.as_str(), "ask_user_question");
        assert!(
            reg.resolve("AskUserQuestion").is_none(),
            "PascalCase spelling must not remain as an alias"
        );
        assert_eq!(question_items["minItems"], 1);
        assert_eq!(question_items["maxItems"], 4);
        assert_eq!(option_items["minItems"], 2);
        assert_eq!(option_items["maxItems"], 4);
        assert_eq!(
            question_items["items"]["required"],
            serde_json::json!(["header", "question", "options"])
        );
        assert_eq!(
            option_items["items"]["required"],
            serde_json::json!(["label", "description"])
        );
        assert_eq!(
            question_items["items"]["properties"]["multiSelect"]["type"],
            "boolean"
        );

        let request = ToolRequest {
            id: "questionnaire".to_string(),
            name: ToolName::plain("ask_user_question"),
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some(
                r#"{"questions":[{"header":"Runtime","question":"Which path?","options":[{"label":"Reuse","description":"Use the runtime broker"},{"label":"New","description":"Create another path"}],"multiSelect":false}]}"#
                    .to_string(),
            ),
        };

        assert!(tool.is_read_only(&request));
        assert!(!tool.is_concurrent_safe(&request));
        let result = reg.execute(&request, &registry::ToolContext::new(Path::new(".")));
        assert_eq!(result.status, ToolStatus::Failed);
        assert!(
            result
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("interactive TUI session")
        );
    }

    #[test]
    fn skill_tools_are_model_visible_readonly_tools() {
        let reg = registry::default_tool_registry();
        for name in ["list_skills", "read_skill"] {
            let tool = reg.get(name).expect("skill tool is registered");
            let request = ToolRequest {
                id: name.to_string(),
                name: ToolName::plain(name),
                action: ActionKind::Read,
                target: None,
                raw_arguments: Some(r#"{"id":"debugging"}"#.to_string()),
            };

            assert!(tool.spec().exposure.is_model_visible());
            assert!(tool.is_read_only(&request));
            assert!(tool.is_concurrent_safe(&request));
        }
    }

    #[cfg(unix)]
    #[test]
    fn mcp_tool_execution_observes_cancel_callback() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        let server = temp_dir.path().join("slow_mcp_server.sh");
        fs::write(
            &server,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"slow","version":"1"}}}\n'
      ;;
    *'"method":"notifications/initialized"'*)
      ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"wait","description":"waits","inputSchema":{"type":"object","properties":{},"required":[]}}]}}\n'
      ;;
    *'"method":"tools/call"'*)
      sleep 5
      printf '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"too late"}],"isError":false}}\n'
      ;;
  esac
done
"#,
        )
        .expect("write MCP fixture");
        {
            use std::os::unix::fs::PermissionsExt;
            let mut permissions = fs::metadata(&server).expect("metadata").permissions();
            permissions.set_mode(0o755);
            fs::set_permissions(&server, permissions).expect("chmod MCP fixture");
        }

        let registry = orca_mcp::initialize_registry(&[McpServerConfig {
            name: "slow".to_string(),
            transport: McpTransportKind::Stdio,
            command: Some(server.to_string_lossy().into_owned()),
            args: Vec::new(),
            url: None,
            env: Default::default(),
            headers: Default::default(),
            disabled: false,
            capabilities: Default::default(),
            startup_timeout_ms: None,
            tool_timeout_ms: None,
        }]);
        assert!(
            registry.errors().is_empty(),
            "registry errors: {:?}",
            registry.errors()
        );
        let request = ToolRequest {
            id: "mcp-call".to_string(),
            name: ToolName::Mcp("mcp__slow__wait".to_string()),
            action: ActionKind::Write,
            target: None,
            raw_arguments: Some("{}".to_string()),
        };
        let cancelled = AtomicBool::new(false);
        let started = Instant::now();

        let result = execute_with_mcp_external_policy_or_cancel(
            &request,
            temp_dir.path(),
            &registry,
            &[],
            ToolOutputTruncation::default(),
            30,
            || {
                if started.elapsed() >= Duration::from_millis(100) {
                    cancelled.store(true, Ordering::SeqCst);
                }
                cancelled.load(Ordering::SeqCst)
            },
        );

        assert!(
            started.elapsed() < Duration::from_millis(750),
            "cancelled MCP call took {:?}",
            started.elapsed()
        );
        assert_eq!(result.status, ToolStatus::Cancelled);
        assert_eq!(
            result.kind,
            orca_core::tool_types::ToolResultKind::Cancelled
        );
        assert_eq!(result.error.as_deref(), Some("MCP tool call cancelled"));
    }

    #[test]
    fn execute_with_mcp_passes_registry_to_resource_tools() {
        let resource = orca_core::mcp_types::McpResource {
            server: "notes".to_string(),
            uri: "memo://orca/one".to_string(),
            name: "memo one".to_string(),
            description: Some("A test memo".to_string()),
            mime_type: Some("text/plain".to_string()),
        };
        let read_result = orca_core::mcp_types::ReadResourceResult {
            contents: vec![orca_core::mcp_types::McpResourceContent {
                uri: "memo://orca/one".to_string(),
                mime_type: Some("text/plain".to_string()),
                text: Some("resource body".to_string()),
                blob: None,
            }],
        };
        let registry = McpRegistry::from_static_resources_for_test(
            vec![resource],
            HashMap::from([(
                ("notes".to_string(), "memo://orca/one".to_string()),
                read_result,
            )]),
        );

        let list = execute_with_mcp(
            &ToolRequest {
                id: "list-resources".to_string(),
                name: ToolName::ListMcpResources,
                action: ActionKind::Read,
                target: None,
                raw_arguments: Some(r#"{"server":"notes"}"#.to_string()),
            },
            Path::new("."),
            &registry,
        );
        assert_eq!(list.status, ToolStatus::Completed);
        assert!(
            list.output
                .as_deref()
                .unwrap_or_default()
                .contains(r#""uri":"memo://orca/one""#)
        );

        let read = execute_with_mcp(
            &ToolRequest {
                id: "read-resource".to_string(),
                name: ToolName::ReadMcpResource,
                action: ActionKind::Read,
                target: None,
                raw_arguments: Some(r#"{"server":"notes","uri":"memo://orca/one"}"#.to_string()),
            },
            Path::new("."),
            &registry,
        );
        assert_eq!(read.status, ToolStatus::Completed);
        assert!(
            read.output
                .as_deref()
                .unwrap_or_default()
                .contains("resource body")
        );
    }

    #[test]
    fn list_mcp_resources_reports_partial_resource_errors() {
        let resource = orca_core::mcp_types::McpResource {
            server: "notes".to_string(),
            uri: "memo://orca/one".to_string(),
            name: "memo one".to_string(),
            description: Some("A test memo".to_string()),
            mime_type: Some("text/plain".to_string()),
        };
        let registry = McpRegistry::from_resource_listing_for_test(
            vec![resource],
            vec!["broken: resources/list timed out".to_string()],
        );

        let result = execute_with_mcp(
            &ToolRequest {
                id: "list-resources".to_string(),
                name: ToolName::ListMcpResources,
                action: ActionKind::Read,
                target: None,
                raw_arguments: Some("{}".to_string()),
            },
            Path::new("."),
            &registry,
        );

        assert_eq!(result.status, ToolStatus::Completed);
        let output: serde_json::Value =
            serde_json::from_str(result.output.as_deref().expect("tool output"))
                .expect("resource listing JSON");
        assert_eq!(output["resources"][0]["server"], "notes");
        assert_eq!(output["resources"][0]["uri"], "memo://orca/one");
        assert_eq!(output["errors"][0], "broken: resources/list timed out");
    }

    #[test]
    fn list_mcp_resources_reports_registry_initialization_errors() {
        let resource = orca_core::mcp_types::McpResource {
            server: "notes".to_string(),
            uri: "memo://orca/one".to_string(),
            name: "memo one".to_string(),
            description: Some("A test memo".to_string()),
            mime_type: Some("text/plain".to_string()),
        };
        let registry = McpRegistry::from_resource_listing_for_test(vec![resource], Vec::new())
            .with_registry_errors_for_test(vec![
                "failed to start MCP server 'broken': boom".to_string(),
            ]);

        let result = execute_with_mcp(
            &ToolRequest {
                id: "list-resources".to_string(),
                name: ToolName::ListMcpResources,
                action: ActionKind::Read,
                target: None,
                raw_arguments: Some("{}".to_string()),
            },
            Path::new("."),
            &registry,
        );

        assert_eq!(result.status, ToolStatus::Completed);
        let output: serde_json::Value =
            serde_json::from_str(result.output.as_deref().expect("tool output"))
                .expect("resource listing JSON");
        assert_eq!(output["resources"][0]["server"], "notes");
        assert_eq!(
            output["errors"][0],
            "failed to start MCP server 'broken': boom"
        );
    }

    #[test]
    fn list_mcp_resource_templates_reports_partial_template_errors() {
        let template = orca_core::mcp_types::McpResourceTemplate {
            server: "docs".to_string(),
            uri_template: "file:///{path}".to_string(),
            name: "workspace file".to_string(),
            description: Some("A file exposed by path".to_string()),
            mime_type: Some("text/plain".to_string()),
        };
        let registry = McpRegistry::from_resource_template_listing_for_test(
            vec![template],
            vec!["broken: resources/templates/list timed out".to_string()],
        );

        let result = execute_with_mcp(
            &ToolRequest {
                id: "list-resource-templates".to_string(),
                name: ToolName::ListMcpResourceTemplates,
                action: ActionKind::Read,
                target: None,
                raw_arguments: Some("{}".to_string()),
            },
            Path::new("."),
            &registry,
        );

        assert_eq!(result.status, ToolStatus::Completed);
        let output: serde_json::Value =
            serde_json::from_str(result.output.as_deref().expect("tool output"))
                .expect("resource template listing JSON");
        assert_eq!(output["resourceTemplates"][0]["server"], "docs");
        assert_eq!(
            output["resourceTemplates"][0]["uriTemplate"],
            "file:///{path}"
        );
        assert_eq!(
            output["errors"][0],
            "broken: resources/templates/list timed out"
        );
    }

    #[test]
    fn list_mcp_resource_templates_reports_registry_initialization_errors() {
        let template = orca_core::mcp_types::McpResourceTemplate {
            server: "docs".to_string(),
            uri_template: "file:///{path}".to_string(),
            name: "workspace file".to_string(),
            description: Some("A file exposed by path".to_string()),
            mime_type: Some("text/plain".to_string()),
        };
        let registry =
            McpRegistry::from_resource_template_listing_for_test(vec![template], Vec::new())
                .with_registry_errors_for_test(vec![
                    "failed to start MCP server 'broken': boom".to_string(),
                ]);

        let result = execute_with_mcp(
            &ToolRequest {
                id: "list-resource-templates".to_string(),
                name: ToolName::ListMcpResourceTemplates,
                action: ActionKind::Read,
                target: None,
                raw_arguments: Some("{}".to_string()),
            },
            Path::new("."),
            &registry,
        );

        assert_eq!(result.status, ToolStatus::Completed);
        let output: serde_json::Value =
            serde_json::from_str(result.output.as_deref().expect("tool output"))
                .expect("resource template listing JSON");
        assert_eq!(output["resourceTemplates"][0]["server"], "docs");
        assert_eq!(
            output["errors"][0],
            "failed to start MCP server 'broken': boom"
        );
    }

    #[test]
    fn external_tool_cannot_shadow_builtin_list_files_alias() {
        let external_tools = vec![ExternalToolConfig {
            name: "list_files".to_string(),
            description: "external list files".to_string(),
            action_kind: ActionKind::Shell,
            command: "echo external".to_string(),
            schema: serde_json::json!({}),
        }];
        let reg = registry::tool_registry_with_mcp_and_external(None, &external_tools);
        let resolved = reg.resolve("list_files").expect("list_files alias");

        assert_eq!(resolved.tool.name(), "glob");
        assert_eq!(resolved.spec.capabilities.action_kind(), ActionKind::Read);
    }

    #[test]
    fn readonly_batch_ignores_caller_supplied_write_action_for_read_tool() {
        let request = ToolRequest {
            id: "read".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Write,
            target: Some("README.md".to_string()),
            raw_arguments: None,
        };

        assert!(should_run_readonly_batch(2, &request));
    }

    #[test]
    fn canonical_action_kind_ignores_caller_supplied_read_for_shell() {
        let request = ToolRequest {
            id: "bash".to_string(),
            name: ToolName::Bash,
            action: ActionKind::Read,
            target: Some("echo hi".to_string()),
            raw_arguments: None,
        };

        assert_eq!(canonical_action_kind(&request), ActionKind::Shell);
    }

    #[test]
    fn registry_executes_glob_with_pattern_and_path() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        fs::create_dir_all(temp_dir.path().join("src/bin")).expect("fixture dir");
        fs::write(temp_dir.path().join("src/lib.rs"), "lib").expect("fixture");
        fs::write(temp_dir.path().join("src/bin/main.rs"), "main").expect("fixture");
        fs::write(temp_dir.path().join("src/readme.md"), "readme").expect("fixture");
        let reg = registry::default_tool_registry();
        let request = ToolRequest {
            id: "glob".to_string(),
            name: ToolName::Glob,
            action: ActionKind::Read,
            target: Some("src".to_string()),
            raw_arguments: Some(r#"{"pattern":"**/*.rs","path":"src"}"#.to_string()),
        };

        let result = reg.execute(&request, &registry::ToolContext::new(temp_dir.path()));

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(
            result.output.as_deref(),
            Some("src/bin/main.rs\nsrc/lib.rs")
        );
    }

    #[test]
    fn registry_executes_glob_with_workspace_prefixed_pattern_from_dot_path() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        fs::create_dir_all(temp_dir.path().join("src/bin")).expect("fixture dir");
        fs::write(temp_dir.path().join("src/lib.rs"), "lib").expect("fixture");
        fs::write(temp_dir.path().join("src/bin/main.rs"), "main").expect("fixture");
        fs::write(temp_dir.path().join("README.md"), "readme").expect("fixture");
        let reg = registry::default_tool_registry();
        let request = ToolRequest {
            id: "glob".to_string(),
            name: ToolName::Glob,
            action: ActionKind::Read,
            target: Some(".".to_string()),
            raw_arguments: Some(r#"{"pattern":"src/**/*.rs","path":"."}"#.to_string()),
        };

        let result = reg.execute(&request, &registry::ToolContext::new(temp_dir.path()));

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(
            result.output.as_deref(),
            Some("src/bin/main.rs\nsrc/lib.rs")
        );
    }

    #[test]
    fn registry_executes_glob_with_no_matches() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        fs::create_dir_all(temp_dir.path()).expect("fixture dir");
        let reg = registry::default_tool_registry();
        let request = ToolRequest {
            id: "glob".to_string(),
            name: ToolName::Glob,
            action: ActionKind::Read,
            target: Some("missing".to_string()),
            raw_arguments: Some(r#"{"pattern":"*.rs","path":"missing"}"#.to_string()),
        };

        let result = reg.execute(&request, &registry::ToolContext::new(temp_dir.path()));

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.output.as_deref(), Some("(no matches)"));
        assert_eq!(
            result.kind,
            orca_core::tool_types::ToolResultKind::NoMatches
        );
    }

    #[test]
    fn registry_executes_glob_with_fuzzy_query() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        fs::create_dir_all(temp_dir.path().join("src/runtime/config")).expect("fixture dir");
        fs::write(temp_dir.path().join("src/runtime/config/mod.rs"), "mod").expect("fixture");
        let reg = registry::default_tool_registry();
        let request = ToolRequest {
            id: "glob".to_string(),
            name: ToolName::Glob,
            action: ActionKind::Read,
            target: None,
            raw_arguments: Some(r#"{"mode":"fuzzy","query":"rcm"}"#.to_string()),
        };

        let result = reg.execute(&request, &registry::ToolContext::new(temp_dir.path()));

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.kind, orca_core::tool_types::ToolResultKind::Success);
        let output = result.output.expect("fuzzy output");
        assert!(
            output
                .lines()
                .any(|line| line == "src/runtime/config/mod.rs")
        );
    }

    #[test]
    fn registry_exposes_glob_fuzzy_schema() {
        let reg = registry::default_tool_registry();
        let glob = reg.resolve("glob").expect("glob tool").tool;
        let schema = &glob.spec().input_schema;

        assert_eq!(schema["properties"]["mode"]["anyOf"][0]["enum"][1], "fuzzy");
        assert_eq!(schema["properties"]["query"]["anyOf"][0]["type"], "string");
        assert_eq!(schema["properties"]["query"]["anyOf"][1]["type"], "null");
        assert!(
            schema["anyOf"]
                .as_array()
                .expect("anyOf")
                .iter()
                .any(|entry| entry["required"]
                    .as_array()
                    .expect("required")
                    .iter()
                    .any(|value| value == "query"))
        );
    }

    #[test]
    fn registry_executes_glob_with_truncated_kind() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        fs::create_dir_all(temp_dir.path().join("src")).expect("fixture dir");
        fs::write(temp_dir.path().join("src/alpha.rs"), "alpha").expect("fixture");
        fs::write(temp_dir.path().join("src/bravo.rs"), "bravo").expect("fixture");
        let request = ToolRequest {
            id: "glob".to_string(),
            name: ToolName::Glob,
            action: ActionKind::Read,
            target: Some("src".to_string()),
            raw_arguments: Some(r#"{"pattern":"*.rs","path":"src"}"#.to_string()),
        };

        let result = glob::execute(&request, temp_dir.path(), 12);

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(
            result.kind,
            orca_core::tool_types::ToolResultKind::Truncated
        );
        assert!(result.truncated);
    }

    #[test]
    fn registry_executes_builtin_tool_by_registered_name() {
        let temp_dir = tempfile::tempdir().expect("temp dir");
        fs::write(temp_dir.path().join("note.txt"), "hello registry\n").expect("fixture");
        let reg = registry::default_tool_registry();
        let request = ToolRequest {
            id: "read".to_string(),
            name: ToolName::ReadFile,
            action: ActionKind::Read,
            target: Some("note.txt".to_string()),
            raw_arguments: Some(r#"{"path":"note.txt"}"#.to_string()),
        };
        let ctx = registry::ToolContext::new(temp_dir.path());

        let result = reg.execute(&request, &ctx);

        assert_eq!(result.status, ToolStatus::Completed);
        assert_eq!(result.output.as_deref(), Some("hello registry\n"));
    }
}
