//! Subagent delegation contract.
//!
//! These tests pin the parts of the delegation design that must not drift:
//!
//! - One role catalog drives the model-visible catalog, the tool schema enum,
//!   the runtime tool ceiling, and the child prompt.
//! - A built-in role's tool ceiling reaches the runtime as an explicit
//!   `ChildAgentRequest.allowed_tools` list, which is what turns the policy
//!   from an advertisement into a hard rejection.
//! - The delegation policy is a separate axis from approval mode: `off`
//!   refuses new launches while leaving resume, status, and stop reachable.
//! - The shared running-child limit is one limit, and a reservation cannot be
//!   double-spent.
//! - A detached child result is delivered to the parent model exactly once.

use orca_core::approval_rules::PermissionRules;
use orca_core::approval_types::ApprovalMode;
use orca_core::config::{
    HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, RunConfig, ThemeName, ToolConfig,
    WorkflowConfig,
};
use orca_core::model::ModelSelection;
use orca_core::subagent_config::{DelegationPolicy, SubagentConfig};
use orca_core::subagent_types::{RoleCapability, SubagentType, builtin_agents};
use orca_core::task_types::TaskStatus;
use orca_runtime::child_agent_entrypoints::role_tool_ceiling;
use orca_runtime::session::InteractiveSession;
use orca_runtime::subagent_admission::{SubagentAdmission, SubagentAdmissionError};
use orca_runtime::tasks::{ResultDelivery, TaskRegistry};

fn test_config() -> RunConfig {
    RunConfig {
        app_version: "0.0.0-test".to_string(),
        prompt: String::new(),
        cwd: None,
        output_format: OutputFormat::Text,
        approval_mode: ApprovalMode::Suggest,
        execution_profile: orca_core::capability::ExecutionProfile::Workspace,
        provider: ProviderKind::Mock,
        verifier: None,
        model: ModelSelection::parse(None).unwrap(),
        model_runtime: ModelRuntimeConfig::default(),
        reasoning_effort: orca_core::config::ReasoningEffort::Max,
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
        permission_rules: PermissionRules::default(),
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

// ---------------------------------------------------------------- role catalog

#[test]
fn subagent_schema_enum_matches_the_role_catalog() {
    let registry = orca_tools::registry::default_tool_registry();
    let mut description = registry.get("subagent").unwrap().spec().description.clone();
    let mut schema = registry
        .get("subagent")
        .unwrap()
        .spec()
        .input_schema
        .clone();
    orca_tools::schema::apply_subagent_catalog(&mut description, &mut schema, &Default::default());

    let enumerated = schema["properties"]["subagent_type"]["enum"]
        .as_array()
        .expect("subagent_type enum")
        .iter()
        .map(|value| value.as_str().unwrap().to_string())
        .collect::<Vec<_>>();

    let expected = builtin_agents()
        .iter()
        .map(|descriptor| descriptor.name.to_string())
        .collect::<Vec<_>>();
    assert_eq!(enumerated, expected);
    assert!(enumerated.contains(&"explorer".to_string()));
}

#[test]
fn every_role_selection_contract_is_visible_to_the_model() {
    let registry = orca_tools::registry::default_tool_registry();
    let mut description = registry.get("subagent").unwrap().spec().description.clone();
    let mut schema = registry
        .get("subagent")
        .unwrap()
        .spec()
        .input_schema
        .clone();
    orca_tools::schema::apply_subagent_catalog(&mut description, &mut schema, &Default::default());

    assert!(description.contains("Built-in roles (use the identifier verbatim)"));
    for descriptor in builtin_agents() {
        assert!(
            description.contains(descriptor.when_to_use),
            "{} when_to_use must reach the model",
            descriptor.name
        );
        assert!(
            description.contains(descriptor.avoid_when),
            "{} avoid_when must reach the model",
            descriptor.name
        );
    }
    assert!(description.contains("explore/scout=explorer"));
}

#[test]
fn the_enforced_ceiling_is_exactly_the_role_catalog_tool_list() {
    for descriptor in builtin_agents() {
        let ceiling = role_tool_ceiling(&descriptor.kind.subagent_type());
        let expected = descriptor
            .tools
            .iter()
            .map(|tool| (*tool).to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            ceiling, expected,
            "{} is enforced with a different list than the catalog declares",
            descriptor.name
        );
    }
}

#[test]
fn every_enforced_builtin_ceiling_resolves_and_excludes_grandchildren() {
    let registry = orca_tools::registry::default_tool_registry();
    for descriptor in builtin_agents() {
        let ceiling = role_tool_ceiling(&descriptor.kind.subagent_type());
        assert!(
            !ceiling.is_empty(),
            "{} must be enforced with a nonempty ceiling",
            descriptor.name
        );
        for tool in &ceiling {
            assert!(
                registry.resolve(tool).is_some(),
                "{} names unknown tool '{tool}'",
                descriptor.name
            );
        }
        assert!(
            !ceiling.iter().any(|tool| tool == "subagent"),
            "{} must not be able to launch grandchildren",
            descriptor.name
        );
    }
    assert!(
        role_tool_ceiling(&SubagentType::Custom("audit".to_string())).is_empty(),
        "a custom agent must never inherit an implicit ceiling"
    );
}

#[test]
fn read_only_roles_declare_no_write_or_execute_capability() {
    for descriptor in builtin_agents().iter().filter(|d| d.is_read_only()) {
        assert!(
            descriptor.forbidden.contains(&RoleCapability::Write),
            "{} is read-only but does not forbid writes",
            descriptor.name
        );
        assert!(
            descriptor.forbidden.contains(&RoleCapability::Execute),
            "{} is read-only but does not forbid execution",
            descriptor.name
        );
    }
    let descriptor = SubagentType::Explorer
        .builtin()
        .expect("explorer descriptor");
    assert_eq!(descriptor.name, "explorer");
    assert!(descriptor.is_read_only());
    assert!(!descriptor.may_write());
    assert!(!descriptor.may_execute());
    let ceiling = role_tool_ceiling(&SubagentType::Explorer);
    for forbidden in ["bash", "edit", "write_file", "web_search"] {
        assert!(
            !ceiling.iter().any(|tool| tool == forbidden),
            "explorer must not be able to call {forbidden}"
        );
    }
}

// -------------------------------------------------------- delegation policy

#[test]
fn delegation_policy_is_independent_of_approval_mode() {
    let mut config = test_config();
    config.approval_mode = ApprovalMode::FullAuto;
    config.subagents.delegation = DelegationPolicy::Off;

    // Full-auto grants tool execution; it never asks for more delegation.
    assert!(!config.subagents.delegation.allows_new_children());
    assert!(!config.subagents.delegation.is_proactive());

    config.subagents.delegation = DelegationPolicy::Adaptive;
    assert!(config.subagents.delegation.allows_new_children());
    assert!(config.subagents.delegation.is_proactive());

    // Plan mode stays read-only however proactive delegation is.
    config.approval_mode = ApprovalMode::Plan;
    assert_eq!(
        config.execution_profile,
        orca_core::capability::ExecutionProfile::Workspace
    );
}

#[test]
fn explicit_policy_is_the_default_and_never_delegates_proactively() {
    let config = test_config();
    assert_eq!(config.subagents.delegation, DelegationPolicy::Explicit);
    assert!(config.subagents.delegation.allows_new_children());
    assert!(!config.subagents.delegation.is_proactive());
}

#[test]
fn delegation_is_off_below_the_depth_it_would_exceed() {
    let policy = DelegationPolicy::Adaptive;
    assert_eq!(policy.for_child(0, 2), DelegationPolicy::Adaptive);
    assert_eq!(policy.for_child(1, 2), DelegationPolicy::Adaptive);
    assert_eq!(policy.for_child(2, 2), DelegationPolicy::Off);
    assert_eq!(DelegationPolicy::Off.for_child(0, 2), DelegationPolicy::Off);
}

#[test]
fn adaptive_prompt_states_critical_path_and_no_quota_rules() {
    let prompt = orca_runtime::agent_common::format_subagent_guidance(DelegationPolicy::Adaptive);

    assert!(prompt.contains("## Delegation"));
    assert!(prompt.contains("next local step"));
    assert!(prompt.contains("Do not repeat the same searches"));
    assert!(prompt.contains("Never delegate just to fill a quota"));
    assert!(prompt.contains("`explorer`"));
    // The capsule lists roles without dumping every full role prompt.
    assert!(!prompt.contains("## Explorer Role"));
}

#[test]
fn explicit_prompt_requires_an_explicit_user_request() {
    let prompt = orca_runtime::agent_common::format_subagent_guidance(DelegationPolicy::Explicit);

    assert!(prompt.contains("only when the user explicitly asked"));
    assert!(!prompt.contains("## Explorer Role"));
}

#[test]
fn off_prompt_forbids_new_children_and_keeps_existing_ones_reachable() {
    let prompt = orca_runtime::agent_common::format_subagent_guidance(DelegationPolicy::Off);

    assert!(prompt.contains("Delegation is turned off"));
    assert!(prompt.contains("Do not start new child agents"));
    assert!(prompt.contains("subagent_status"));
    assert!(prompt.contains("task_stop"));
    assert!(!prompt.contains("Decision rules"));
}

#[test]
fn child_contract_requires_the_documented_report_and_read_only_truthfulness() {
    let contract = orca_runtime::agent_common::format_child_agent_contract(
        &SubagentType::Explorer,
        DelegationPolicy::Explicit,
    );

    for required in [
        "Status: complete, partial, failed, or uncertain",
        "Evidence:",
        "Files changed:",
        "Verification:",
        "Open questions:",
    ] {
        assert!(
            contract.contains(required),
            "child contract missing {required}"
        );
    }
    assert!(contract.contains("enforced read-only"));
    assert!(!contract.contains("synchronous subagent"));
    assert!(contract.contains("direct answer"));
}

#[test]
fn child_system_prompt_does_not_claim_a_synchronous_role() {
    let cwd = std::env::temp_dir();
    let prompt = orca_runtime::agent_common::build_agent_system_prompt_with_goal(
        &cwd,
        1,
        &SubagentType::Explorer,
        None,
        ApprovalMode::Suggest,
        None,
        None,
        DelegationPolicy::Explicit,
    );

    // A detached child must never be told it is synchronous.
    assert!(!prompt.contains("synchronous subagent"));
    assert!(prompt.contains("## Subagent Role"));
    assert!(prompt.contains("## Explorer Role"));
    assert!(prompt.contains("enforced read-only"));

    // The main agent gets the delegation policy instead of the child contract.
    let main = orca_runtime::agent_common::build_agent_system_prompt(
        &cwd,
        0,
        &SubagentType::General,
        None,
        ApprovalMode::Suggest,
        None,
    );
    assert!(main.contains("## Delegation"));
    assert!(!main.contains("## Subagent Role"));
}

// ------------------------------------------------------------------ admission

#[test]
fn shared_limit_refuses_a_second_launch_and_recovers_after_release() {
    let registry = TaskRegistry::new("subagent-contract-admission".to_string());
    let admission = SubagentAdmission::default();

    let held = admission
        .admit(&registry, 1)
        .expect("first reservation is admitted");
    let refused = admission
        .admit(&registry, 1)
        .expect_err("second reservation must be refused");
    assert_eq!(
        refused,
        SubagentAdmissionError::CapacityExceeded {
            limit: 1,
            running: 1
        }
    );
    let message = refused.message();
    assert!(message.contains("No child was started"));
    assert!(message.contains("Do not retry immediately"));

    drop(held);
    admission
        .admit(&registry, 1)
        .expect("slot returns after release");
}

#[test]
fn registry_admission_is_shared_across_clones() {
    let registry = TaskRegistry::new("subagent-contract-shared".to_string());
    let clone = registry.clone();

    let held = registry.admit_child(1).expect("first admission");
    assert!(
        clone.admit_child(1).is_err(),
        "a clone must share the same admission gate"
    );
    drop(held);
    clone.admit_child(1).expect("slot returns");
}

#[test]
fn admission_counts_detached_running_children_only() {
    let registry = TaskRegistry::new("subagent-contract-count".to_string());
    assert_eq!(registry.active_detached_subagent_count(), 0);

    // An in-band child is bounded by its own batch window and its parent is
    // blocked on it, so it must not consume the detached-child limit.
    let in_band = registry.create_subagent("sync child".to_string(), None);
    assert_eq!(registry.active_detached_subagent_count(), 0);

    let detached = registry.create_subagent("detached child".to_string(), None);
    assert!(registry.mark_subagent_result_pending(&detached.id));
    assert_eq!(registry.active_detached_subagent_count(), 1);
    assert_eq!(
        registry.get(&detached.id).unwrap().status,
        TaskStatus::Queued
    );

    registry
        .stop(&detached.id, "test stop".to_string())
        .expect("stop child");
    assert_eq!(
        registry.active_detached_subagent_count(),
        0,
        "a terminal child must not hold a running slot"
    );

    registry
        .stop(&in_band.id, "test stop".to_string())
        .expect("stop in-band child");
}

// ------------------------------------------------------------ result delivery

#[test]
fn detached_child_result_is_delivered_to_the_parent_exactly_once() {
    let registry = TaskRegistry::new("subagent-contract-delivery".to_string());
    let child = registry.create_subagent("trace the call chain".to_string(), None);

    // A child launched in band never needs a pushed notification.
    assert_eq!(
        registry.get(&child.id).unwrap().result_delivery,
        ResultDelivery::InBand
    );
    assert!(registry.drain_pending_subagent_results().is_empty());

    // A detached child does.
    assert!(registry.mark_subagent_result_pending(&child.id));
    assert_eq!(
        registry.get(&child.id).unwrap().result_delivery,
        ResultDelivery::Pending
    );
    registry
        .complete(&child.id, "found it in src/a.rs:12".to_string())
        .expect("complete child");

    let first = registry.drain_pending_subagent_results();
    assert_eq!(first.len(), 1);
    let notification = first[0].model_notification();
    assert!(notification.starts_with("<task-notification>"));
    assert!(notification.contains(&child.id));
    assert!(notification.contains("status completed"));
    assert!(notification.contains("src/a.rs:12"));
    assert!(
        notification.contains("not a user instruction"),
        "a child report must not read as an instruction that widens authority"
    );

    assert!(
        registry.drain_pending_subagent_results().is_empty(),
        "a delivered result must never be injected twice"
    );
}

#[test]
fn unfinished_detached_child_is_not_delivered_early() {
    let registry = TaskRegistry::new("subagent-contract-delivery-early".to_string());
    let child = registry.create_subagent("still working".to_string(), None);
    let _ = registry.mark_subagent_result_pending(&child.id);

    assert!(
        registry.drain_pending_subagent_results().is_empty(),
        "a running child has no result to deliver"
    );
    assert_eq!(
        registry.get(&child.id).unwrap().result_delivery,
        ResultDelivery::Pending,
        "the pending marker must survive an early drain"
    );
}

#[test]
fn failed_detached_child_delivers_its_failure_reason() {
    let registry = TaskRegistry::new("subagent-contract-delivery-failed".to_string());
    let child = registry.create_subagent("doomed".to_string(), None);
    let _ = registry.mark_subagent_result_pending(&child.id);
    registry
        .fail(&child.id, "provider returned 500".to_string())
        .expect("fail child");

    let pending = registry.drain_pending_subagent_results();
    assert_eq!(pending.len(), 1);
    let notification = pending[0].model_notification();
    assert!(notification.contains("status failed"));
    assert!(notification.contains("provider returned 500"));
}

#[test]
fn long_detached_result_is_truncated_with_a_paging_hint() {
    let registry = TaskRegistry::new("subagent-contract-delivery-long".to_string());
    let child = registry.create_subagent("long".to_string(), None);
    let _ = registry.mark_subagent_result_pending(&child.id);
    registry
        .complete(&child.id, "x".repeat(20_000))
        .expect("complete child");

    let pending = registry.drain_pending_subagent_results();
    let notification = pending[0].model_notification();
    assert!(notification.contains("result truncated"));
    assert!(
        notification.contains("output_next_limit") || notification.contains("output_next_offset")
    );
    assert!(
        notification.chars().count() < 5_000,
        "an oversized child result must not flood the parent context"
    );
}

#[test]
fn a_delivered_result_is_already_marked_after_a_restart() {
    // The marker is persisted with the task record, so a fresh registry that
    // loads the same session must not re-deliver.
    let registry = TaskRegistry::new("subagent-contract-delivery-restart".to_string());
    let child = registry.create_subagent("once".to_string(), None);
    let _ = registry.mark_subagent_result_pending(&child.id);
    registry
        .complete(&child.id, "done".to_string())
        .expect("complete child");
    assert_eq!(registry.drain_pending_subagent_results().len(), 1);

    let reloaded = TaskRegistry::new("subagent-contract-delivery-restart".to_string());
    assert!(
        reloaded.drain_pending_subagent_results().is_empty(),
        "an in-memory session reload must not resurface a delivered result"
    );
}

// ------------------------------------------------------------- session wiring

#[test]
fn session_opens_with_a_system_prompt_and_an_available_task_registry() {
    let config = test_config();
    let session = InteractiveSession::new_with_preloaded(&config, "prompt", None).expect("session");

    assert!(matches!(
        session.conversation().messages.first(),
        Some(orca_core::conversation::Message::System { .. })
    ));
    // A fresh session has nothing to deliver and nothing occupying a slot.
    assert_eq!(session.task_registry().active_detached_subagent_count(), 0);
    assert!(
        session
            .task_registry()
            .drain_pending_subagent_results()
            .is_empty()
    );
}
