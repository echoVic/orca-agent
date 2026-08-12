use std::io;

use orca_approval::prompt_user;
use orca_core::approval_types::{ApprovalDecision, ApprovalRequest, ApprovalResolution};
use orca_core::config::{OutputFormat, RunConfig};
use orca_core::tool_types;

pub(crate) fn resolve_interactive(
    config: &RunConfig,
    approval: &ApprovalRequest,
    tool_request: &tool_types::ToolRequest,
) -> io::Result<ApprovalResolution> {
    if config.output_format == OutputFormat::Jsonl {
        return Ok(ApprovalResolution {
            id: approval.id.clone(),
            decision: ApprovalDecision::Deny,
            reason: "interactive confirmation unavailable in jsonl mode".to_string(),
        });
    }

    let allowed = prompt_user(tool_request.name.as_str(), tool_request.target.as_deref())?;

    Ok(ApprovalResolution {
        id: approval.id.clone(),
        decision: if allowed {
            ApprovalDecision::Allow
        } else {
            ApprovalDecision::Deny
        },
        reason: if allowed {
            "user approved".to_string()
        } else {
            "user denied".to_string()
        },
    })
}

#[cfg(test)]
mod tests {
    use orca_core::approval_types::{ActionKind, ApprovalDecision, ApprovalRequest};
    use orca_core::config::{OutputFormat, ProviderKind, RunConfig};
    use orca_core::model::ModelSelection;
    use orca_core::tool_types;

    fn config(output_format: OutputFormat) -> RunConfig {
        RunConfig {
            app_version: "0.0.0-test".to_string(),
            prompt: String::new(),
            cwd: None,
            output_format,
            approval_mode: orca_core::approval_types::ApprovalMode::Suggest,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::parse(None).unwrap(),
            model_runtime: Default::default(),
            reasoning_effort: orca_core::config::ReasoningEffort::Max,
            api_key: None,
            base_url: None,
            history_mode: orca_core::config::HistoryMode::Disabled,
            show_session_picker: false,
            active_permission_profile: None,
            permission_profiles: Default::default(),
            runtime_workspace_roots: None,
            permission_rules: Default::default(),
            additional_working_directories: Vec::new(),
            budget: Default::default(),
            mcp_servers: Vec::new(),
            external_tools: Vec::new(),
            hooks: Vec::new(),
            subagents: Default::default(),
            tools: Default::default(),
            workflows: Default::default(),
            theme: orca_core::config::ThemeName::Dark,
            vim_mode: false,
            vim_insert_escape: None,
            update_check: false,
            desktop_notifications: false,
            terminal_notifications: false,
            auto_memory: false,
        }
    }

    #[test]
    fn jsonl_mode_denies_interactive_approval_without_prompting() {
        let approval = ApprovalRequest {
            id: "approval-1".to_string(),
            action: ActionKind::Write,
            description: "edit tracked.txt".to_string(),
            tool: Some("edit".to_string()),
            target: Some("tracked.txt".to_string()),
            preview: None,
        };
        let request = tool_types::ToolRequest {
            id: "edit".to_string(),
            name: tool_types::ToolName::Edit,
            action: ActionKind::Write,
            target: Some("tracked.txt".to_string()),
            raw_arguments: None,
        };

        let resolution =
            super::resolve_interactive(&config(OutputFormat::Jsonl), &approval, &request)
                .expect("approval resolution");

        assert_eq!(resolution.id, "approval-1");
        assert_eq!(resolution.decision, ApprovalDecision::Deny);
        assert_eq!(
            resolution.reason,
            "interactive confirmation unavailable in jsonl mode"
        );
    }
}
