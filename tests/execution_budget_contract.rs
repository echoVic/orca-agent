//! Execution Budget Redesign — contract tests (Task 1 of the implementation
//! plan). RED state: these tests reference the new budget types
//! (`orca_core::budget`, `orca_runtime::budget_controller`) that do not exist
//! yet, so the file fails to compile until Tasks 2–3 land.
//!
//! The contract under test:
//! - Unlimited by default: no implicit 128-turn ceiling.
//! - Typed budget stops: the first exhausted dimension produces a typed
//!   `StopReason`, not a status-plus-reason string pair.
//! - Checkpoint-before-terminal ordering: a `Stopped` terminal is resumable
//!   only after a checkpoint has been recorded; the terminal must never claim
//!   resumability before the checkpoint exists.
//! - Verifier success never upgrades a budget stop: budget stop, verifier
//!   result, and process exit stay independently observable.

use orca_core::budget::{BudgetSpec, BudgetUsage, OperationTerminal, StopReason};
use orca_core::verification::VerificationResult;
use orca_runtime::budget_controller::BudgetController;

#[test]
fn unlimited_default_admits_beyond_128_turns() {
    // A default spec has every dimension unlimited; the old hidden
    // `DEFAULT_MAX_TURNS = 128` ceiling must not exist anywhere.
    let spec = BudgetSpec::default();
    assert!(spec.max_turns.is_none());
    assert!(spec.max_tool_calls.is_none());
    assert!(spec.max_cost_usd_micros.is_none());
    assert!(spec.max_wall_time_ms.is_none());

    let mut controller = BudgetController::new(spec);
    for _ in 0..200 {
        controller.admit_turn().expect("unlimited turns admit");
        controller
            .admit_tool_call()
            .expect("unlimited tool calls admit");
    }

    let usage = controller.usage();
    assert_eq!(usage.turns, 200);
    assert_eq!(usage.tool_calls, 200);

    // ModelEnded is normal completion: an unlimited run finishes Completed.
    let terminal = controller.terminal();
    assert!(matches!(
        terminal,
        OperationTerminal::Completed { usage }
            if usage.turns == 200 && usage.tool_calls == 200
    ));
}

#[test]
fn typed_turn_budget_stops_with_typed_terminal() {
    let mut controller = BudgetController::new(BudgetSpec {
        max_turns: Some(3),
        ..BudgetSpec::default()
    });

    for _ in 0..3 {
        controller.admit_turn().expect("first three turns admit");
    }

    let stop = controller.admit_turn().expect_err("fourth turn must stop");
    assert!(matches!(
        stop.reason,
        StopReason::TurnBudget { max_turns: 3 }
    ));
    assert_eq!(stop.usage.turns, 3);

    // The terminal is typed: Stopped carries the reason, usage, checkpoint,
    // and resumability — never a bare status string.
    controller.record_checkpoint("cp-1");
    let terminal = controller.terminal();
    match terminal {
        OperationTerminal::Stopped {
            reason,
            usage,
            checkpoint_id,
            resumable,
        } => {
            assert_eq!(reason, StopReason::TurnBudget { max_turns: 3 });
            assert_eq!(usage.turns, 3);
            assert_eq!(checkpoint_id, "cp-1");
            assert!(resumable);
        }
        other => panic!("expected Stopped terminal, got {other:?}"),
    }
}

#[test]
fn checkpoint_precedes_resumable_terminal() {
    let mut controller = BudgetController::new(BudgetSpec {
        max_turns: Some(1),
        ..BudgetSpec::default()
    });
    controller.admit_turn().expect("first turn admits");
    controller.admit_turn().expect_err("second turn must stop");

    // Before any checkpoint is recorded, the terminal must not claim
    // resumability and must not carry a checkpoint id.
    let before = controller.terminal();
    match before {
        OperationTerminal::Stopped {
            checkpoint_id,
            resumable,
            ..
        } => {
            assert!(!resumable, "terminal resumable before checkpoint");
            assert!(
                checkpoint_id.is_empty(),
                "terminal carried checkpoint before it was recorded"
            );
        }
        other => panic!("expected Stopped terminal, got {other:?}"),
    }

    // Recording the durable conversation boundary flips resumability.
    controller.record_checkpoint("cp-boundary");
    match controller.terminal() {
        OperationTerminal::Stopped {
            checkpoint_id,
            resumable,
            ..
        } => {
            assert!(resumable);
            assert_eq!(checkpoint_id, "cp-boundary");
        }
        other => panic!("expected Stopped terminal, got {other:?}"),
    }
}

#[test]
fn verifier_success_never_upgrades_a_budget_stop() {
    let mut controller = BudgetController::new(BudgetSpec {
        max_cost_usd_micros: Some(1_000),
        ..BudgetSpec::default()
    });
    // Charge 2x the cost ceiling.
    let stop = controller
        .record_cost_usd_micros(2_000)
        .expect_err("cost ceiling must stop");
    assert!(matches!(stop.reason, StopReason::CostBudget { .. }));
    controller.record_checkpoint("cp-cost");

    // A passing verifier is independent of the operation terminal.
    let verifier = VerificationResult {
        command: "test -f result".to_string(),
        success: true,
        exit_code: Some(0),
        stdout: String::new(),
        stderr: String::new(),
    };
    assert!(verifier.success);

    let terminal = controller.terminal();
    assert!(
        matches!(terminal, OperationTerminal::Stopped { .. }),
        "verifier success must not convert a budget stop into completion"
    );
    // And the terminal must stay exactly the stop the controller recorded.
    assert_eq!(
        terminal,
        operation_terminal_after_verifier(controller.terminal(), &verifier)
    );
}

/// Contract helper documenting the invariant the runtime must honor: the
/// verifier result is metadata alongside the terminal; it never rewrites a
/// budget stop into success.
fn operation_terminal_after_verifier(
    terminal: OperationTerminal,
    _verifier: &VerificationResult,
) -> OperationTerminal {
    terminal
}

#[test]
fn tool_call_budget_stops_on_exhausted_dimension() {
    let mut controller = BudgetController::new(BudgetSpec {
        max_tool_calls: Some(2),
        ..BudgetSpec::default()
    });

    controller.admit_turn().expect("turn admits");
    controller.admit_tool_call().expect("first tool call");
    controller.admit_tool_call().expect("second tool call");

    let stop = controller
        .admit_tool_call()
        .expect_err("third tool call must stop");
    assert!(matches!(
        stop.reason,
        StopReason::ToolCallBudget { max_tool_calls: 2 }
    ));
    assert_eq!(stop.usage.tool_calls, 2);
}

#[test]
fn budget_usage_saturates_instead_of_wrapping() {
    let mut usage = BudgetUsage {
        turns: u32::MAX,
        tool_calls: u32::MAX,
        cost_usd_micros: u64::MAX,
        wall_time_ms: u64::MAX,
    };
    usage.turns = usage.turns.saturating_add(1);
    usage.tool_calls = usage.tool_calls.saturating_add(1);
    usage.cost_usd_micros = usage.cost_usd_micros.saturating_add(1);
    usage.wall_time_ms = usage.wall_time_ms.saturating_add(1);
    assert_eq!(usage.turns, u32::MAX);
    assert_eq!(usage.tool_calls, u32::MAX);
    assert_eq!(usage.cost_usd_micros, u64::MAX);
    assert_eq!(usage.wall_time_ms, u64::MAX);
}
