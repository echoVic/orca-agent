use std::path::PathBuf;

use orca_core::config::file::{ConfigOverrides, FileConfig, load_effective_config};
use orca_core::config::{BudgetConfig, HistoryMode, OutputFormat, ProviderKind, RunConfig};
use orca_core::model::ModelSelection;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DesktopNotifications {
    #[default]
    FromConfig,
    Disabled,
}

#[derive(Clone, Debug)]
pub struct RunConfigRequest {
    pub app_version: String,
    pub config_cwd: PathBuf,
    pub runtime_cwd: Option<PathBuf>,
    pub prompt: String,
    pub output_format: OutputFormat,
    pub provider: ProviderKind,
    pub verifier: Option<String>,
    pub history_mode: HistoryMode,
    pub show_session_picker: bool,
    pub budget: BudgetConfig,
    pub overrides: ConfigOverrides,
    pub desktop_notifications: DesktopNotifications,
}

impl RunConfigRequest {
    pub fn new(app_version: impl Into<String>, config_cwd: PathBuf) -> Self {
        Self {
            app_version: app_version.into(),
            config_cwd,
            runtime_cwd: None,
            prompt: String::new(),
            output_format: OutputFormat::Text,
            provider: ProviderKind::DeepSeek,
            verifier: None,
            history_mode: HistoryMode::Record,
            show_session_picker: false,
            budget: BudgetConfig::default(),
            overrides: ConfigOverrides::default(),
            desktop_notifications: DesktopNotifications::FromConfig,
        }
    }
}

pub fn build_run_config(request: RunConfigRequest) -> Result<RunConfig, String> {
    let file = load_effective_config(&request.config_cwd, request.overrides.clone())?;
    assemble_run_config(request, file)
}

pub fn assemble_run_config(
    request: RunConfigRequest,
    file: FileConfig,
) -> Result<RunConfig, String> {
    let model = ModelSelection::parse(file.model.clone())?;
    let desktop_notifications = match request.desktop_notifications {
        DesktopNotifications::FromConfig => file.desktop_notifications,
        DesktopNotifications::Disabled => false,
    };

    Ok(RunConfig {
        app_version: request.app_version,
        prompt: request.prompt,
        cwd: request.runtime_cwd,
        output_format: request.output_format,
        approval_mode: file.mode.unwrap_or_default(),
        provider: request.provider,
        verifier: request.verifier,
        model,
        model_runtime: file.model_runtime,
        reasoning_effort: file.reasoning_effort,
        api_key: file.api_key,
        base_url: file.base_url,
        history_mode: request.history_mode,
        show_session_picker: request.show_session_picker,
        active_permission_profile: None,
        permission_profiles: file.permission_profiles,
        runtime_workspace_roots: None,
        permission_rules: file.permissions,
        additional_working_directories: Vec::new(),
        budget: {
            // CLI arguments take precedence over file configuration: the
            // request budget is the base and file values fill only the
            // dimensions the caller did not explicitly set.
            let mut budget = request.budget;
            if budget.max_turns.is_none() {
                budget.max_turns = file.budget.max_turns;
            }
            if budget.max_tool_calls.is_none() {
                budget.max_tool_calls = file.budget.max_tool_calls;
            }
            if budget.max_cost_usd_micros.is_none() {
                budget.max_cost_usd_micros = file.budget.max_cost_usd_micros;
            }
            if budget.max_wall_time_ms.is_none() {
                budget.max_wall_time_ms = file.budget.max_wall_time_ms;
            }
            budget.validate()?;
            budget
        },
        mcp_servers: file.mcp_servers,
        hooks: file.hooks,
        external_tools: orca_tools::external::load_default_external_tools(),
        subagents: file.subagents.normalized(),
        tools: file.tools.normalized(),
        workflows: file.workflows.resolved(),
        theme: file.theme,
        vim_mode: file.vim_mode,
        vim_insert_escape: file.vim_insert_escape,
        update_check: file.update_check,
        desktop_notifications,
        terminal_notifications: file.terminal_notifications,
        auto_memory: file.auto_memory,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use orca_core::approval_types::ApprovalMode;
    use orca_core::config::file::{ConfigOverrides, FileConfig};
    use orca_core::config::{HistoryMode, OutputFormat, ProviderKind};

    use super::*;

    #[test]
    fn missing_file_mode_defaults_to_auto_edit() {
        let request = RunConfigRequest::new("0.3.4", PathBuf::from("/workspace"));

        let config = assemble_run_config(request, FileConfig::default()).unwrap();

        assert_eq!(config.approval_mode, ApprovalMode::AutoEdit);
    }

    #[test]
    fn assembles_shared_run_config_without_losing_launch_fields() {
        let file = FileConfig {
            mode: Some(ApprovalMode::Plan),
            vim_insert_escape: Some(
                orca_core::config::VimInsertEscapeSequence::parse("jj").unwrap(),
            ),
            update_check: false,
            desktop_notifications: true,
            auto_memory: true,
            ..FileConfig::default()
        };
        let request = RunConfigRequest {
            app_version: "9.8.7".to_string(),
            config_cwd: PathBuf::from("/config-root"),
            runtime_cwd: Some(PathBuf::from("/runtime-root")),
            prompt: "inspect".to_string(),
            output_format: OutputFormat::Jsonl,
            provider: ProviderKind::Mock,
            verifier: Some("cargo test".to_string()),
            history_mode: HistoryMode::Resume("latest".to_string()),
            show_session_picker: true,
            budget: BudgetConfig {
                max_cost_usd_micros: Some(2_500_000),
                ..BudgetConfig::default()
            },
            overrides: ConfigOverrides::default(),
            desktop_notifications: DesktopNotifications::FromConfig,
        };

        let config = assemble_run_config(request, file).unwrap();

        assert_eq!(config.app_version, "9.8.7");
        assert_eq!(config.cwd, Some(PathBuf::from("/runtime-root")));
        assert_eq!(config.prompt, "inspect");
        assert_eq!(config.output_format, OutputFormat::Jsonl);
        assert_eq!(config.approval_mode, ApprovalMode::Plan);
        assert_eq!(config.provider, ProviderKind::Mock);
        assert_eq!(config.verifier.as_deref(), Some("cargo test"));
        assert!(matches!(
            config.history_mode,
            HistoryMode::Resume(ref selector) if selector == "latest"
        ));
        assert!(config.show_session_picker);
        assert_eq!(config.budget.max_cost_usd_micros, Some(2_500_000));
        assert!(!config.update_check);
        assert_eq!(
            config
                .vim_insert_escape
                .as_ref()
                .map(|value| value.as_str()),
            Some("jj")
        );
        assert!(config.desktop_notifications);
        assert!(config.auto_memory);
    }

    #[test]
    fn cli_budget_overrides_file_budget_dimension_wise() {
        let file = FileConfig {
            budget: BudgetConfig {
                max_turns: Some(16),
                max_cost_usd_micros: Some(1_000_000),
                ..BudgetConfig::default()
            },
            ..FileConfig::default()
        };
        let request = RunConfigRequest {
            // CLI explicitly sets max_turns=8; cost stays from the file.
            budget: BudgetConfig {
                max_turns: Some(8),
                ..BudgetConfig::default()
            },
            ..RunConfigRequest::new("0.3.0", PathBuf::from("/workspace"))
        };

        let config = assemble_run_config(request, file).unwrap();
        assert_eq!(config.budget.max_turns, Some(8));
        assert_eq!(config.budget.max_cost_usd_micros, Some(1_000_000));
        assert_eq!(config.budget.max_tool_calls, None);
    }

    #[test]
    fn zero_budget_dimension_is_rejected_with_clear_error() {
        let request = RunConfigRequest {
            budget: BudgetConfig {
                max_turns: Some(0),
                ..BudgetConfig::default()
            },
            ..RunConfigRequest::new("0.3.0", PathBuf::from("/workspace"))
        };

        let error = assemble_run_config(request, FileConfig::default())
            .expect_err("zero max_turns must be rejected");
        assert!(error.contains("max_turns"));
    }

    #[test]
    fn launch_can_disable_desktop_notifications_without_changing_file_config() {
        let file = FileConfig {
            desktop_notifications: true,
            ..FileConfig::default()
        };
        let request = RunConfigRequest {
            desktop_notifications: DesktopNotifications::Disabled,
            ..RunConfigRequest::new("0.2.55", PathBuf::from("/workspace"))
        };

        let config = assemble_run_config(request, file).unwrap();

        assert!(!config.desktop_notifications);
    }
}
