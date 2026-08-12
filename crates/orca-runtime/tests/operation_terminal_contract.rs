//! Typed operation terminal contract (Task 6): surfaces consume the same
//! typed `OperationTerminal` object; adapters must not reconstruct budget
//! facts from constants; terminal ordering and independent verification
//! metadata are enforced.

use orca_core::budget::{BudgetSpec, BudgetStop, BudgetUsage, OperationTerminal, StopReason};
use orca_core::event_schema::{EventFactory, RunStatus};
use orca_runtime::budget_controller::BudgetController;
use orca_runtime::lifecycle::{
    AgentLoopResult, RuntimeSessionLifecycle, RuntimeTaskActor, RuntimeTaskKind,
};

#[test]
fn agent_loop_budget_stop_carries_typed_terminal() {
    let stop = BudgetStop {
        reason: StopReason::TurnBudget { max_turns: 3 },
        usage: BudgetUsage {
            turns: 3,
            tool_calls: 2,
            cost_usd_micros: 0,
            wall_time_ms: 0,
        },
    };
    let result = AgentLoopResult::budget_stop(stop, "cp-1".to_string());

    // The typed terminal is the fact; legacy status/reason are projections.
    match result.terminal.as_ref() {
        Some(OperationTerminal::Stopped {
            reason,
            usage,
            checkpoint_id,
            resumable,
        }) => {
            assert_eq!(*reason, StopReason::TurnBudget { max_turns: 3 });
            assert_eq!(usage.turns, 3);
            assert_eq!(usage.tool_calls, 2);
            assert_eq!(checkpoint_id, "cp-1");
            assert!(*resumable);
        }
        other => panic!("expected typed Stopped terminal, got {other:?}"),
    }
    // Exit code 4 comes from the typed terminal, never from RunStatus.
    assert_eq!(
        result
            .terminal
            .as_ref()
            .expect("terminal")
            .clone()
            .exit_code(),
        4
    );
}

#[test]
fn run_status_no_longer_carries_budget_exhaustion() {
    // The deleted variant is gone: budget stops map through the typed
    // terminal, and RunStatus keeps only plain statuses.
    assert_eq!(RunStatus::Failed.as_str(), "failed");
    assert_eq!(RunStatus::Failed.exit_code(), 1);
    let variants = [
        "success",
        "failed",
        "cancelled",
        "approval_required",
        "verification_failed",
    ];
    for variant in variants {
        let status: RunStatus = serde_json::from_str(&format!("\"{variant}\""))
            .expect("legacy RunStatus wire variants stay parseable");
        assert_eq!(status.as_str(), variant);
    }
}

#[test]
fn session_completed_terminal_emits_typed_terminal_payload() {
    let mut events = EventFactory::new("terminal-event".to_string());
    let terminal = OperationTerminal::Stopped {
        reason: StopReason::CostBudget {
            max_cost_usd_micros: 1_250_000,
        },
        usage: BudgetUsage {
            turns: 2,
            tool_calls: 1,
            cost_usd_micros: 1_300_000,
            wall_time_ms: 42,
        },
        checkpoint_id: "cp-9".to_string(),
        resumable: true,
    };
    let event = events.session_completed_terminal(&terminal, Some("session-9"));

    // Status string is a projection of the terminal; the typed object rides
    // along so adapters never reconstruct limits from constants.
    assert_eq!(event.payload["status"], "budget_exhausted");
    assert_eq!(event.payload["session_id"], "session-9");
    let terminal_payload = &event.payload["terminal"]["stopped"];
    assert_eq!(
        terminal_payload["reason"]["cost_budget"]["max_cost_usd_micros"],
        1_250_000
    );
    assert_eq!(terminal_payload["usage"]["turns"], 2);
    assert_eq!(terminal_payload["checkpoint_id"], "cp-9");
    assert_eq!(terminal_payload["resumable"], true);
}

#[test]
fn controller_terminal_ordering_matches_journal_contract() {
    // A controller stop is only resumable after the checkpoint is recorded —
    // the same ordering the execution journal enforces durably.
    let mut controller = BudgetController::new(BudgetSpec {
        max_turns: Some(2),
        ..BudgetSpec::default()
    });
    controller.admit_turn().expect("turn 1");
    controller.admit_turn().expect("turn 2");
    let stop = controller.admit_turn().expect_err("turn 3 stops");

    let before = controller.terminal();
    match before {
        OperationTerminal::Stopped { resumable, .. } => {
            assert!(!resumable, "terminal must not be resumable pre-checkpoint")
        }
        other => panic!("expected Stopped, got {other:?}"),
    }

    controller.record_checkpoint("cp-boundary");
    match controller.terminal() {
        OperationTerminal::Stopped {
            reason,
            checkpoint_id,
            resumable,
            ..
        } => {
            assert_eq!(reason, stop.reason);
            assert_eq!(checkpoint_id, "cp-boundary");
            assert!(resumable);
        }
        other => panic!("expected Stopped, got {other:?}"),
    }
}

#[test]
fn verification_metadata_is_independent_of_terminal() {
    // A budget stop stays a stop even when verification metadata says the
    // work passed; the two are separate facts.
    let mut controller = BudgetController::new(BudgetSpec {
        max_cost_usd_micros: Some(1_000),
        ..BudgetSpec::default()
    });
    controller
        .record_cost_usd_micros(2_000)
        .expect_err("cost stop");
    controller.record_checkpoint("cp-cost");
    let terminal = controller.terminal();

    let verifier_passed = orca_core::verification::VerificationResult {
        command: "true".to_string(),
        success: true,
        exit_code: Some(0),
        stdout: String::new(),
        stderr: String::new(),
    };
    assert!(verifier_passed.success);

    // No conversion path: the terminal object is untouched by verification.
    assert!(matches!(terminal, OperationTerminal::Stopped { .. }));
    assert!(!matches!(terminal, OperationTerminal::Completed { .. }));
}

#[test]
fn runtime_actor_no_longer_enforces_hidden_ceiling() {
    let mut lifecycle = RuntimeSessionLifecycle::new("no-ceiling");
    lifecycle.start_task(RuntimeTaskKind::Agent);
    let mut actor = RuntimeTaskActor::new(&mut lifecycle);
    let mut events = EventFactory::new("no-ceiling".to_string());
    let turn_id = orca_core::thread_identity::TurnId::new();
    for turn in 1..=150 {
        let started = actor
            .start_turn(&mut events, &turn_id, Some("hello"), true)
            .expect("unlimited turns");
        assert_eq!(started.turn(), turn);
    }
}
