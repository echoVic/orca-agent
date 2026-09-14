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
use orca_runtime::child_agent_entrypoints::role_tool_ceiling;
use orca_runtime::session::InteractiveSession;
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
fn every_enforced_builtin_ceiling_resolves_and_only_general_can_delegate() {
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
        assert_eq!(
            ceiling.iter().any(|tool| tool == "subagent"),
            descriptor.name == "general",
            "{} has an unexpected nested delegation capability",
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
fn adaptive_is_the_default_and_delegates_proactively() {
    let config = test_config();
    assert_eq!(config.subagents.delegation, DelegationPolicy::Adaptive);
    assert!(config.subagents.delegation.allows_new_children());
    assert!(config.subagents.delegation.is_proactive());
}

#[test]
fn explicit_policy_never_delegates_proactively() {
    let policy = DelegationPolicy::Explicit;
    assert!(policy.allows_new_children());
    assert!(!policy.is_proactive());
    let prompt = orca_runtime::agent_common::format_subagent_guidance(policy);
    assert!(prompt.contains("only when the user explicitly asked"));
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
    assert!(prompt.contains("task_list"));
    assert!(prompt.contains("task_wait"));
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
fn a_full_execution_pool_queues_the_next_child_instead_of_refusing_it() {
    use orca_core::subagent_config::SubagentLimits;
    use orca_runtime::execution_scope::Admission;

    let registry = TaskRegistry::new("subagent-contract-queue".to_string());
    let limits = SubagentLimits {
        max_running: 1,
        max_queued: 8,
        max_live_tasks: 32,
    };
    let scope = registry.execution_scope(&limits);

    let first = registry.create_subagent("first".to_string(), None);
    assert_eq!(scope.classify(&first.id), Some(Admission::Started));
    registry.mark_running(&first.id).expect("running");

    // The second submission is accepted and waits; it is not a tool failure.
    let second = registry.create_subagent("second".to_string(), None);
    assert_eq!(scope.classify(&second.id), Some(Admission::Queued));
    assert!(
        scope.refuses().is_none(),
        "capacity pressure is a queue position, not a refusal"
    );

    let capacity = scope.capacity();
    assert_eq!(capacity.running, 1);
    assert_eq!(capacity.queued, 1);
    assert_eq!(capacity.limits.max_running, 1);
    assert_eq!(capacity.to_json()["limit"], 1);
    assert_eq!(capacity.to_json()["queued"], 1);
}

#[test]
fn capacity_is_shared_by_clones_of_one_registry() {
    use orca_core::subagent_config::SubagentLimits;

    let registry = TaskRegistry::new("subagent-contract-shared".to_string());
    let clone = registry.clone();
    let limits = SubagentLimits {
        max_running: 1,
        max_queued: 4,
        max_live_tasks: 16,
    };

    let first = registry.create_subagent("first".to_string(), None);
    registry.mark_running(&first.id).expect("running");

    let scope = clone.execution_scope(&limits);
    assert_eq!(
        scope.running(),
        1,
        "a clone must see the same durable execution leases"
    );
    assert_eq!(scope.ready_to_start(0), Vec::<String>::new());
}

#[test]
fn a_saturated_scope_refuses_only_at_a_real_boundary() {
    use orca_core::subagent_config::SubagentLimits;
    use orca_runtime::execution_scope::AdmissionRefusal;

    let registry = TaskRegistry::new("subagent-contract-refusal".to_string());
    let limits = SubagentLimits {
        max_running: 1,
        max_queued: 2,
        max_live_tasks: 64,
    };
    let scope = registry.execution_scope(&limits);

    let running = registry.create_subagent("running".to_string(), None);
    registry.mark_running(&running.id).expect("running");
    registry.create_subagent("queued-one".to_string(), None);
    registry.create_subagent("queued-two".to_string(), None);

    let refusal = scope.refuses().expect("the queue is at its limit");
    assert_eq!(
        refusal,
        AdmissionRefusal::QueueFull {
            queued: 2,
            limit: 2
        }
    );
    let message = refusal.message();
    assert!(message.contains("was not accepted"));
    assert!(
        message.contains("task_stop"),
        "a refusal must say what still works: {message}"
    );
}

#[test]
fn the_running_count_tracks_execution_leases_not_delivery() {
    let registry = TaskRegistry::new("subagent-contract-count".to_string());
    assert_eq!(registry.active_subagent_lease_count(), 0);

    // A queued child is waiting for a lease, not holding one.
    let child = registry.create_subagent("child".to_string(), None);
    assert_eq!(
        registry.active_subagent_lease_count(),
        0,
        "a queued child holds no execution lease"
    );
    registry.mark_running(&child.id).expect("running");
    assert_eq!(
        registry.active_subagent_lease_count(),
        1,
        "a running child holds one"
    );
    // A child that released its lease while waiting for its own children is
    // still non-terminal, and still does not hold a lease.
    assert!(registry.request_pause(&child.id).is_err());
    assert_eq!(registry.active_subagent_lease_count(), 1);
    assert!(
        registry.requeue_for_resume(&child.id, orca_runtime::task_view::WaitReason::Dependency)
    );
    assert_eq!(registry.active_subagent_lease_count(), 0);

    registry
        .stop(&child.id, "test stop".to_string())
        .expect("stop child");
    assert_eq!(
        registry.active_subagent_lease_count(),
        0,
        "a terminal child must not hold a running slot"
    );

    // Delivery state is a separate axis: whether the parent still owes this
    // result a notification never changes how much execution capacity is used.
    let delivered = registry.create_subagent("delivered".to_string(), None);
    let _ = registry.mark_subagent_result_pending(&delivered.id);
    registry
        .complete(&delivered.id, "done".to_string())
        .expect("complete");
    assert_eq!(registry.active_subagent_lease_count(), 0);
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
    assert!(registry.claim_pending_subagent_results().is_empty());

    // A detached child does.
    assert!(registry.mark_subagent_result_pending(&child.id));
    assert_eq!(
        registry.get(&child.id).unwrap().result_delivery,
        ResultDelivery::Pending
    );
    registry
        .complete(&child.id, "found it in src/a.rs:12".to_string())
        .expect("complete child");

    let first = registry.claim_pending_subagent_results();
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
        registry.claim_pending_subagent_results().is_empty(),
        "a delivered result must never be injected twice"
    );
}

#[test]
fn unfinished_detached_child_is_not_delivered_early() {
    let registry = TaskRegistry::new("subagent-contract-delivery-early".to_string());
    let child = registry.create_subagent("still working".to_string(), None);
    let _ = registry.mark_subagent_result_pending(&child.id);

    assert!(
        registry.claim_pending_subagent_results().is_empty(),
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

    let pending = registry.claim_pending_subagent_results();
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

    let pending = registry.claim_pending_subagent_results();
    let notification = pending[0].model_notification();
    assert!(notification.contains("result truncated"));
    assert!(
        notification.contains("task_read_output") && notification.contains("next_cursor"),
        "a truncated notification must name the tool and cursor that read the rest: {notification}"
    );
    assert!(
        notification.chars().count() < 5_000,
        "an oversized child result must not flood the parent context"
    );
}

#[test]
fn a_claimed_result_is_not_delivered_until_it_is_acknowledged() {
    // Claiming is not delivering: a crash between the claim and the parent
    // conversation must retry, not lose the notification.
    let registry = TaskRegistry::new("subagent-contract-delivery-claim".to_string());
    let child = registry.create_subagent("once".to_string(), None);
    let _ = registry.mark_subagent_result_pending(&child.id);
    registry
        .complete(&child.id, "done".to_string())
        .expect("complete child");

    let claimed = registry.claim_pending_subagent_results();
    assert_eq!(claimed.len(), 1);
    assert!(
        !claimed[0].claim_id.is_empty(),
        "a claim must carry the identity the acknowledgement returns"
    );

    // The claim is durable: a second turn sees it is in flight and does not
    // inject the same result concurrently.
    assert!(
        registry.claim_pending_subagent_results().is_empty(),
        "an unacknowledged but live claim must not be handed out twice"
    );
    assert_eq!(
        registry.outstanding_subagent_results().len(),
        1,
        "an unacknowledged result is still owed to the parent"
    );

    // The acknowledgement closes it.
    let ack = orca_runtime::tasks::DeliveryAck::from(&claimed[0]);
    assert!(registry.ack_subagent_result(&ack));
    assert!(
        registry.claim_pending_subagent_results().is_empty(),
        "an acknowledged result must never be injected again"
    );
    assert!(registry.outstanding_subagent_results().is_empty());
}

#[test]
fn a_replayed_acknowledgement_cannot_mark_a_newer_result_delivered() {
    let registry = TaskRegistry::new("subagent-contract-delivery-replay".to_string());
    let child = registry.create_subagent("once".to_string(), None);
    let _ = registry.mark_subagent_result_pending(&child.id);
    registry
        .complete(&child.id, "first".to_string())
        .expect("complete child");

    let claimed = registry.claim_pending_subagent_results();
    let stale_ack = orca_runtime::tasks::DeliveryAck::from(&claimed[0]);
    assert!(registry.ack_subagent_result(&stale_ack));

    // A second, replayed acknowledgement with the same identity is rejected.
    assert!(
        !registry.ack_subagent_result(&stale_ack),
        "a replayed acknowledgement must not be accepted"
    );
    // An acknowledgement for a different result revision is rejected too.
    let wrong_revision = orca_runtime::tasks::DeliveryAck {
        task_id: stale_ack.task_id.clone(),
        result_revision: stale_ack.result_revision + 1,
        claim_id: stale_ack.claim_id.clone(),
    };
    assert!(!registry.ack_subagent_result(&wrong_revision));
}

#[test]
fn a_released_claim_makes_the_result_deliverable_again() {
    let registry = TaskRegistry::new("subagent-contract-delivery-release".to_string());
    let child = registry.create_subagent("once".to_string(), None);
    let _ = registry.mark_subagent_result_pending(&child.id);
    registry
        .complete(&child.id, "done".to_string())
        .expect("complete child");

    let claimed = registry.claim_pending_subagent_results();
    let ack = orca_runtime::tasks::DeliveryAck::from(&claimed[0]);
    // The parent conversation could not be written: the result must not be lost.
    assert!(registry.release_subagent_result_claim(&ack));
    let reclaimed = registry.claim_pending_subagent_results();
    assert_eq!(
        reclaimed.len(),
        1,
        "a released claim must be retried by the next turn"
    );
    assert_eq!(reclaimed[0].result_revision, claimed[0].result_revision);
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
    assert_eq!(session.task_registry().active_subagent_lease_count(), 0);
    assert!(
        session
            .task_registry()
            .claim_pending_subagent_results()
            .is_empty()
    );
}
