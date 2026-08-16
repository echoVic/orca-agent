use std::collections::BTreeSet;

use crate::types::UserAction;

// Exact bytes of the reviewed runtime-surface manifest are the test fixture.
const MANIFEST: &str = include_str!(
    "../../../docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json"
);
const CURRENT_ACTIONS: [(&str, &str); 36] = [
    ("StartSideConversation", "host_session_lifecycle_mutation"),
    ("ToggleSideConversation", "host_session_lifecycle_mutation"),
    ("CloseSideConversation", "host_session_lifecycle_mutation"),
    ("NewSession", "host_session_lifecycle_mutation"),
    ("ForkCurrentSession", "host_session_lifecycle_mutation"),
    ("RenameCurrentSession", "host_store_mutation"),
    ("ResumeSavedSession", "host_session_lifecycle_mutation"),
    ("ForkSavedSession", "host_session_lifecycle_mutation"),
    ("RenameSavedSession", "host_store_mutation"),
    ("ArchiveSavedSession", "host_store_mutation"),
    ("DeleteSavedSession", "host_store_mutation"),
    ("Submit", "runtime_mutation"),
    ("SubmitWithMentions", "runtime_mutation"),
    ("SubmitQueued", "runtime_mutation"),
    ("ImplementApprovedPlan", "runtime_mutation"),
    ("SubmitWorkflowNotification", "runtime_mutation"),
    ("RunWorkflow", "workflow_mutation"),
    ("SetModel", "settings_mutation"),
    ("Remember", "host_store_and_thread_mutation"),
    ("Compact", "runtime_mutation"),
    ("GoalShow", "authoritative_read"),
    ("GoalSet", "goal_and_operation_mutation"),
    ("GoalEdit", "goal_mutation"),
    ("GoalClear", "goal_mutation"),
    ("GoalPause", "goal_and_operation_mutation"),
    ("GoalResume", "goal_session_operation_mutation"),
    ("ResolveBackgroundApproval", "interaction_mutation"),
    ("StopTask", "task_mutation"),
    ("ForegroundTask", "task_ownership_mutation"),
    ("RespondToInteraction", "interaction_mutation"),
    ("Backtrack", "history_mutation"),
    ("BackgroundCurrentTurn", "operation_ownership_mutation"),
    ("Interrupt", "operation_mutation"),
    ("Cancel", "host_lifecycle_mutation"),
    ("ResumeOperation", "recovery_mutation"),
    ("CancelOperation", "recovery_mutation"),
];

const FUTURE_ACTIONS: [&str; 0] = [];

fn current_user_action_name(action: &UserAction) -> &'static str {
    match action {
        UserAction::NewSession => "NewSession",
        UserAction::StartSideConversation { .. } => "StartSideConversation",
        UserAction::ToggleSideConversation => "ToggleSideConversation",
        UserAction::CloseSideConversation => "CloseSideConversation",
        UserAction::ForkCurrentSession { .. } => "ForkCurrentSession",
        UserAction::RenameCurrentSession { .. } => "RenameCurrentSession",
        UserAction::ResumeSavedSession { .. } => "ResumeSavedSession",
        UserAction::ForkSavedSession { .. } => "ForkSavedSession",
        UserAction::RenameSavedSession { .. } => "RenameSavedSession",
        UserAction::ArchiveSavedSession { .. } => "ArchiveSavedSession",
        UserAction::DeleteSavedSession { .. } => "DeleteSavedSession",
        UserAction::Submit(_) => "Submit",
        UserAction::SubmitWithMentions { .. } => "SubmitWithMentions",
        UserAction::SubmitQueued { .. } => "SubmitQueued",
        UserAction::ImplementApprovedPlan { .. } => "ImplementApprovedPlan",
        UserAction::SubmitWorkflowNotification(_) => "SubmitWorkflowNotification",
        UserAction::RunWorkflow { .. } => "RunWorkflow",
        UserAction::SetModel(_) => "SetModel",
        UserAction::Remember { .. } => "Remember",
        UserAction::Compact => "Compact",
        UserAction::GoalShow => "GoalShow",
        UserAction::GoalSet(_) => "GoalSet",
        UserAction::GoalEdit(_) => "GoalEdit",
        UserAction::GoalClear => "GoalClear",
        UserAction::GoalPause => "GoalPause",
        UserAction::GoalResume => "GoalResume",
        UserAction::ResolveBackgroundApproval { .. } => "ResolveBackgroundApproval",
        UserAction::StopTask { .. } => "StopTask",
        UserAction::ForegroundTask { .. } => "ForegroundTask",
        UserAction::RespondToInteraction { .. } => "RespondToInteraction",
        UserAction::Backtrack => "Backtrack",
        UserAction::BackgroundCurrentTurn => "BackgroundCurrentTurn",
        UserAction::Interrupt => "Interrupt",
        UserAction::Cancel => "Cancel",
        UserAction::ResumeOperation { .. } => "ResumeOperation",
        UserAction::CancelOperation { .. } => "CancelOperation",
    }
}

const TUI_ENTRYPOINTS: [&str; 37] = [
    "slash.new",
    "slash.model_write",
    "slash.model_read",
    "slash.mode_plan_and_backtab",
    "slash.config_show",
    "slash.cost",
    "slash.goal",
    "slash.workflow_run",
    "slash.workflow_and_agent_panels",
    "slash.skills_list",
    "slash.dynamic_skill",
    "slash.remember",
    "slash.compact",
    "slash.resume",
    "slash.trust_show",
    "slash.trust_mutation",
    "slash_menu.discovery",
    "dispatcher.route_action",
    "approval_always",
    "background_approval_reconstruction",
    "workflow_result_autosubmit",
    "background_task_callbacks",
    "recovered_background_scan",
    "startup_session_mcp",
    "session_picker_transition",
    "goal_callbacks",
    "mention_catalog_expansion",
    "setup_api_key",
    "app_state_update",
    "input_history",
    "terminal_clipboard_notifications",
    "renderer_runtime_events",
    "renderer_frame",
    "terminal_session_startup",
    "renderer_input_wake",
    "renderer_input_routing",
    "renderer_interaction_acks",
];

#[test]
fn runtime_surface_contract_user_actions_are_exactly_classified_with_required_recovery_variants() {
    let _exhaustive_inventory = current_user_action_name as fn(&UserAction) -> &'static str;
    let manifest: serde_json::Value = serde_json::from_str(MANIFEST).expect("manifest JSON");
    let rows = manifest["tui_actions"].as_array().expect("tui_actions");
    let current_rows: Vec<(&str, &str)> = rows
        .iter()
        .filter(|row| row[1] == "current")
        .map(|row| {
            (
                row[0].as_str().expect("action id"),
                row[3].as_str().expect("action classification"),
            )
        })
        .collect();
    assert_eq!(current_rows, CURRENT_ACTIONS);
    assert_eq!(
        manifest["closed_inventory"]["current_tui_user_actions"]
            .as_array()
            .expect("closed current actions")
            .iter()
            .map(|value| value.as_str().expect("closed current action"))
            .collect::<Vec<_>>(),
        CURRENT_ACTIONS
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>(),
        "closed current actions must exactly match the current action rows"
    );
}

#[test]
fn no_future_recovery_actions_remain() {
    let manifest: serde_json::Value = serde_json::from_str(MANIFEST).expect("manifest JSON");
    let rows = manifest["tui_actions"].as_array().expect("tui_actions");
    let additions: Vec<&str> = rows
        .iter()
        .filter(|row| row[1] == "required_addition")
        .map(|row| row[0].as_str().expect("future action id"))
        .collect();

    assert_eq!(additions, FUTURE_ACTIONS);
    assert_eq!(
        manifest["closed_inventory"]["required_tui_user_action_additions"]
            .as_array()
            .expect("required additions")
            .iter()
            .map(|value| value.as_str().expect("required addition"))
            .collect::<Vec<_>>(),
        FUTURE_ACTIONS
    );
}

#[test]
fn mutation_capable_entrypoints_have_a_closed_baseline_route() {
    let manifest: serde_json::Value = serde_json::from_str(MANIFEST).expect("manifest JSON");
    let rows = manifest["tui_entrypoints"]
        .as_array()
        .expect("tui_entrypoints");
    let ids: Vec<&str> = rows
        .iter()
        .map(|row| row[0].as_str().expect("entrypoint id"))
        .collect();

    assert_eq!(ids, TUI_ENTRYPOINTS);
    assert_eq!(ids.iter().collect::<BTreeSet<_>>().len(), ids.len());
    for row in rows {
        let classification = row[2].as_str().expect("entrypoint classification");
        let mutation_capable = classification.contains("mutation")
            || classification.contains("authority")
            || classification.contains("router")
            || classification.contains("transition")
            || classification.contains("runtime_effect");
        if mutation_capable {
            assert!(!row[4].as_str().expect("target route").is_empty());
            assert!(!row[6].as_str().expect("result consumer").is_empty());
            assert!(!row[7].as_str().expect("Phase 3 disposition").is_empty());
        }
    }
}
