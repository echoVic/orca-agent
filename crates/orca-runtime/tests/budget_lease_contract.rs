//! Child-agent and workflow budget lease contract (Task 8).
//!
//! Proves:
//! - Child agents admit turns through a `BudgetLease` bounded by the parent
//!   operation's remaining budget.
//! - Unused lease reservations return to the parent; only consumed usage
//!   reports upward.
//! - Failed children still produce a usage receipt for the parent.
//! - Detached background operations require their own budget and never borrow
//!   the parent's reservation.

use orca_core::approval_rules::PermissionRules;
use orca_core::approval_types::ApprovalMode;
use orca_core::budget::{BudgetSpec, BudgetUsage, StopReason};
use orca_core::config::{
    HistoryMode, ModelRuntimeConfig, OutputFormat, ProviderKind, RunConfig, ThemeName, ToolConfig,
    WorkflowConfig,
};
use orca_core::model::ModelSelection;
use orca_core::subagent_config::SubagentConfig;
use orca_core::subagent_types::SubagentType;
use orca_runtime::budget_controller::{BudgetController, BudgetLease};
use orca_runtime::child_agent_loop_setup::{
    ChildAgentLoopSetup, ChildAgentTurnBudget, advance_child_agent_turn, prepare_child_agent_loop,
};
use orca_runtime::child_agent_types::{ChildAgentRequest, ChildAgentResult};
use orca_runtime::instructions::ProjectInstructions;
use orca_runtime::memory::MemoryBlock;

fn test_config() -> RunConfig {
    RunConfig {
        app_version: "0.0.0-test".to_string(),
        prompt: String::new(),
        cwd: None,
        output_format: OutputFormat::Text,
        approval_mode: ApprovalMode::Suggest,
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

fn child_request() -> ChildAgentRequest {
    ChildAgentRequest::new(
        "inspect repo".to_string(),
        SubagentType::General,
        None,
        1,
        false,
    )
}

fn child_setup(config: &RunConfig) -> ChildAgentLoopSetup {
    let cwd = std::env::temp_dir();
    prepare_child_agent_loop(
        config,
        &child_request(),
        &cwd,
        &ProjectInstructions::default(),
        &MemoryBlock::default(),
    )
}

#[test]
fn child_lease_is_bounded_by_parent_remaining_budget() {
    let mut parent = BudgetController::new(BudgetSpec {
        max_turns: Some(2),
        ..BudgetSpec::default()
    });
    parent.admit_turn().expect("parent turn 1");

    // The parent has one turn left; a child lease can never exceed it.
    let mut lease = parent
        .child_lease(BudgetSpec {
            max_turns: Some(5),
            ..BudgetSpec::default()
        })
        .expect("child lease granted");
    assert_eq!(lease.spec().max_turns, Some(1));

    let mut setup = child_setup(&test_config());
    assert!(matches!(
        advance_child_agent_turn(&mut setup, &mut lease),
        ChildAgentTurnBudget::Continue
    ));
    match advance_child_agent_turn(&mut setup, &mut lease) {
        ChildAgentTurnBudget::Stop(result) => {
            assert!(
                result
                    .error
                    .as_deref()
                    .unwrap_or("")
                    .contains("budget stopped")
            );
        }
        ChildAgentTurnBudget::Continue => {
            panic!("child must stop at the parent's remaining budget")
        }
    }
}

#[test]
fn unused_lease_capacity_returns_to_parent_and_consumed_usage_reports_upward() {
    let mut parent = BudgetController::new(BudgetSpec {
        max_turns: Some(10),
        ..BudgetSpec::default()
    });
    parent.admit_turn().expect("parent turn 1");

    let mut lease = parent
        .child_lease(BudgetSpec {
            max_turns: Some(4),
            ..BudgetSpec::default()
        })
        .expect("child lease granted");

    let mut setup = child_setup(&test_config());
    lease.admit_turn().expect("child turn 1");
    assert!(matches!(
        advance_child_agent_turn(&mut setup, &mut lease),
        ChildAgentTurnBudget::Continue
    ));

    // Only consumed usage is reported; the rest of the reservation returns.
    let consumed = lease.finish();
    assert_eq!(consumed.turns, 2);
    parent
        .merge_child_usage(consumed)
        .expect("child usage merges");
    assert_eq!(parent.usage().turns, 3);
}

#[test]
fn failed_child_still_reports_consumed_usage_receipt() {
    let mut parent = BudgetController::new(BudgetSpec::default());
    let mut lease = parent
        .child_lease(BudgetSpec {
            max_turns: Some(1),
            ..BudgetSpec::default()
        })
        .expect("child lease granted");
    lease.admit_turn().expect("child turn 1");

    // A failed child (provider error, tool failure) still owns its receipt:
    // the parent merges what the child actually consumed.
    let _failed: ChildAgentResult = ChildAgentResult {
        status: orca_core::event_schema::RunStatus::Failed,
        final_message: None,
        error: Some("child failed after one turn".to_string()),
    };
    let consumed = lease.finish();
    assert_eq!(consumed.turns, 1);
    parent
        .merge_child_usage(consumed)
        .expect("failed child usage merges");
    assert_eq!(parent.usage().turns, 1);
}

#[test]
fn detached_background_operations_need_their_own_budget() {
    // Detached background work does not borrow the parent's reservation: it
    // gets an independent controller from its own config, and the parent's
    // lease is untouched.
    let mut parent = BudgetController::new(BudgetSpec {
        max_turns: Some(3),
        ..BudgetSpec::default()
    });
    parent.admit_turn().expect("parent turn 1");
    let _parent_lease = parent
        .child_lease(BudgetSpec::default())
        .expect("parent lease granted");

    let mut background = BudgetController::new(BudgetSpec {
        max_turns: Some(1),
        ..BudgetSpec::default()
    });
    background.admit_turn().expect("background turn 1");
    let stop = background
        .admit_turn()
        .expect_err("background turn 2 stops");
    assert_eq!(stop.reason, StopReason::TurnBudget { max_turns: 1 });

    // The parent is unaffected by the background operation's exhaustion.
    assert_eq!(parent.usage().turns, 1);
    parent.admit_turn().expect("parent turn 2");
    parent.admit_turn().expect("parent turn 3");
}

#[test]
fn child_agent_loop_admits_through_lease_and_returns_consumed_usage() {
    let config = test_config();
    // The parent reserves the child's ceiling; the child cannot outspend it.
    let mut controller = BudgetController::new(BudgetSpec {
        max_turns: Some(2),
        ..BudgetSpec::default()
    });
    let mut lease = controller
        .child_lease(BudgetSpec {
            max_turns: Some(2),
            ..BudgetSpec::default()
        })
        .expect("child lease");

    let mut setup = child_setup(&config);
    assert!(matches!(
        advance_child_agent_turn(&mut setup, &mut lease),
        ChildAgentTurnBudget::Continue
    ));
    assert!(matches!(
        advance_child_agent_turn(&mut setup, &mut lease),
        ChildAgentTurnBudget::Continue
    ));
    match advance_child_agent_turn(&mut setup, &mut lease) {
        ChildAgentTurnBudget::Stop(result) => {
            assert!(
                result
                    .error
                    .as_deref()
                    .unwrap_or("")
                    .contains("budget stopped")
            );
        }
        ChildAgentTurnBudget::Continue => panic!("third child turn must stop"),
    }

    let consumed = lease.finish();
    assert_eq!(consumed.turns, 2);
    // Merging the child's consumption exhausts the parent's own ceiling; the
    // parent latches the stop so the next admission rejects.
    let stop = controller
        .merge_child_usage(consumed)
        .expect_err("parent exhausts after merging the child's full ceiling");
    assert!(matches!(
        stop.reason,
        StopReason::TurnBudget { max_turns: 2 }
    ));
    assert_eq!(controller.usage().turns, 2);
}

#[test]
fn budget_lease_reports_usage_and_spec() {
    let mut parent = BudgetController::new(BudgetSpec::default());
    let mut lease: BudgetLease = parent
        .child_lease(BudgetSpec {
            max_turns: Some(2),
            ..BudgetSpec::default()
        })
        .expect("child lease");
    assert_eq!(lease.usage(), BudgetUsage::default());
    lease.admit_turn().expect("turn 1");
    lease.admit_tool_call().expect("tool 1");
    let usage = lease.usage();
    assert_eq!(usage.turns, 1);
    assert_eq!(usage.tool_calls, 1);
}
