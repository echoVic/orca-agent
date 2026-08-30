use std::collections::HashMap;
use std::path::Path;

use orca_core::approval_types::ApprovalMode;
use orca_core::config::{
    HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, RunConfig, ThemeName, ToolConfig,
    WorkflowConfig,
};
use orca_core::model::ModelSelection;
use orca_core::subagent_config::SubagentConfig;
use orca_runtime::onboarding::{acknowledge_first_run_in, inspect_first_run_in};

fn config(cwd: &Path) -> RunConfig {
    RunConfig {
        app_version: "test".to_string(),
        prompt: String::new(),
        cwd: Some(cwd.to_path_buf()),
        output_format: OutputFormat::Text,
        approval_mode: ApprovalMode::AutoEdit,
        provider: ProviderKind::Mock,
        verifier: None,
        model: ModelSelection::parse(None).expect("default model"),
        model_runtime: ModelRuntimeConfig::default(),
        reasoning_effort: Default::default(),
        api_key: Some("key-from-environment-or-auth".to_string()),
        base_url: None,
        mcp_servers: Vec::new(),
        hooks: Vec::new(),
        external_tools: Vec::new(),
        history_mode: HistoryMode::Disabled,
        show_session_picker: false,
        active_permission_profile: None,
        permission_profiles: HashMap::new(),
        runtime_workspace_roots: None,
        permission_rules: Default::default(),
        additional_working_directories: Vec::new(),
        budget: Default::default(),
        subagents: SubagentConfig::default(),
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

#[test]
fn first_run_acknowledgement_is_bound_to_workspace_and_security_policy() {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    let mut config = config(workspace.path());

    let first = inspect_first_run_in(&config, home.path()).expect("inspect first run");
    assert!(
        !first.acknowledged,
        "an API key must not skip the security disclosure"
    );
    assert_eq!(first.workspace, workspace.path().canonicalize().unwrap());
    assert_eq!(
        first.auth_path,
        home.path().canonicalize().unwrap().join("auth.json")
    );

    acknowledge_first_run_in(&first).expect("persist acknowledgement");
    assert!(
        inspect_first_run_in(&config, home.path())
            .expect("reinspect")
            .acknowledged,
        "the same workspace and policy should not prompt twice"
    );

    config.approval_mode = ApprovalMode::FullAuto;
    assert!(
        !inspect_first_run_in(&config, home.path())
            .expect("inspect changed policy")
            .acknowledged,
        "a security-policy change must reopen the disclosure"
    );
}

#[test]
fn acknowledgement_does_not_grant_workspace_trust() {
    let home = tempfile::tempdir().expect("home");
    let workspace = tempfile::tempdir().expect("workspace");
    let state = inspect_first_run_in(&config(workspace.path()), home.path()).expect("inspect");

    assert!(!state.workspace_trusted);
    acknowledge_first_run_in(&state).expect("acknowledge");

    let reread = inspect_first_run_in(&config(workspace.path()), home.path()).expect("reinspect");
    assert!(reread.acknowledged);
    assert!(
        !reread.workspace_trusted,
        "onboarding acknowledgement must not mutate the trust allowlist"
    );
}
