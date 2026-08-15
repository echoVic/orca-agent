mod action_dispatcher;
mod agent_runtime;
pub mod app;
mod approval_actions;
mod approval_dialog_actions;
mod approval_mode_actions;
mod attachment_routing;
mod background_approval;
mod background_tasks;
pub mod bridge;
mod capability_backend;
mod channels;
pub mod cli;
mod clipboard;
pub mod commands;
mod composer_input_actions;
mod composer_textarea;
pub mod diff;
mod diff_highlight;
mod display_text;
mod edit_highlight;
mod edit_highlight_worker;
mod exit_policy;
mod frame_scheduler;
mod global_actions;
mod hosted_goal;
mod hosted_runtime;
mod hosted_session;
mod hosted_session_lifecycle;
mod hosted_settings;
mod hosted_side;
mod hosted_submission;
mod idle_key_actions;
mod idle_navigation_actions;
mod idle_submit_actions;
mod input_adapter;
mod input_event_actions;
mod input_history;
mod input_runtime;
mod input_wake;
mod insert_escape;
mod key_event_actions;
mod mention_menu_actions;
mod mention_search_manager;
mod operation_controller;
mod plan_approval_actions;
mod plan_panel;
mod presentation;
mod queued_input;
mod queued_input_actions;
mod running_actions;
mod runtime_event_actions;
mod scrollback;
mod selection;
mod session_picker;
mod session_picker_actions;
mod setup_actions;
pub mod shortcuts;
mod slash_command_actions;
mod slash_menu_actions;
mod status_key_actions;
mod stdio_guard;
mod streaming_markdown;
mod submitted_turn;
mod surface_actions;
#[cfg(test)]
mod surface_boundary_tests;
mod surface_client;
mod surface_projection;
mod syntax_highlight;
mod terminal_capabilities;
mod terminal_presentation;
pub mod theme;
mod transcript_search;
mod transcript_view;
pub mod types;
pub mod ui;
pub mod vim;
mod vim_command;
mod workflow_notifications;
mod workflow_panel;
mod workflow_panel_actions;
mod workspace_config;
mod workspace_status;

pub use app::run_tui;

#[cfg(test)]
pub(crate) mod test_support {
    use std::ffi::OsString;
    use std::path::Path;
    use std::sync::{Mutex, MutexGuard};

    use orca_core::approval_types::ApprovalMode;
    use orca_core::config::{
        HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, ReasoningEffort, RunConfig,
        ThemeName, ToolConfig, WorkflowConfig,
    };
    use orca_core::model::ModelSelection;

    static PROCESS_ENV_LOCK: Mutex<()> = Mutex::new(());

    pub(crate) fn lock_process_env() -> MutexGuard<'static, ()> {
        PROCESS_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) struct OrcaHomeGuard {
        _lock: MutexGuard<'static, ()>,
        home: tempfile::TempDir,
        previous: Option<OsString>,
    }

    impl OrcaHomeGuard {
        pub(crate) fn path(&self) -> &Path {
            self.home.path()
        }
    }

    impl Drop for OrcaHomeGuard {
        fn drop(&mut self) {
            unsafe {
                if let Some(previous) = &self.previous {
                    std::env::set_var("ORCA_HOME", previous);
                } else {
                    std::env::remove_var("ORCA_HOME");
                }
            }
        }
    }

    pub(crate) fn isolate_orca_home() -> OrcaHomeGuard {
        let lock = lock_process_env();
        let home = tempfile::tempdir().expect("temporary ORCA_HOME");
        let previous = std::env::var_os("ORCA_HOME");
        unsafe {
            std::env::set_var("ORCA_HOME", home.path());
        }
        OrcaHomeGuard {
            _lock: lock,
            home,
            previous,
        }
    }

    pub(crate) fn test_run_config() -> RunConfig {
        RunConfig {
            app_version: "0.0.0-test".to_string(),
            prompt: String::new(),
            cwd: std::env::current_dir().ok(),
            output_format: OutputFormat::Text,
            approval_mode: ApprovalMode::Suggest,
            provider: ProviderKind::Mock,
            verifier: None,
            model: ModelSelection::from_unchecked(Some("auto".to_string())),
            model_runtime: ModelRuntimeConfig::default(),
            reasoning_effort: ReasoningEffort::Max,
            api_key: None,
            base_url: None,
            mcp_servers: Vec::new(),
            hooks: Vec::new(),
            external_tools: Vec::new(),
            history_mode: HistoryMode::Disabled,
            show_session_picker: false,
            active_permission_profile: None,
            permission_profiles: Default::default(),
            runtime_workspace_roots: None,
            permission_rules: Default::default(),
            additional_working_directories: Vec::new(),
            budget: Default::default(),
            subagents: Default::default(),
            tools: ToolConfig::default(),
            workflows: WorkflowConfig::default(),
            theme: ThemeName::Dark,
            vim_mode: false,
            vim_insert_escape: None,
            update_check: false,
            desktop_notifications: false,
            terminal_notifications: false,
            auto_memory: false,
        }
    }
}
