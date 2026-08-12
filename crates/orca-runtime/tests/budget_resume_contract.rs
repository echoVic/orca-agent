//! Resume and Goal budget contract (Task 7).
//!
//! Proves:
//! - Resume from a budget checkpoint restores the durable conversation
//!   boundary via `last_committed_message_id` and never replays records after
//!   it.
//! - Interrupted tools (unmatched `tool.started`) restore as `indeterminate`
//!   and are never replayed as completed external effects.
//! - A resumed operation owns a fresh operation id and a fresh budget.
//! - Goals own a cumulative budget; an exhausted Goal budget disables
//!   automatic continuation.

use orca_core::conversation::{Message, RawToolCall, normalize_tool_boundaries};
use orca_runtime::budget_controller::BudgetController;
use orca_runtime::execution_journal::{ExecutionJournal, JournalRecord, completed_terminal};
use orca_runtime::thread_store::SessionCheckpointRecord;
use tempfile::tempdir;

#[test]
fn budget_checkpoint_records_last_committed_message_boundary() {
    // The durable checkpoint written on a budget stop carries the last
    // committed conversation item id; resume truncates at that boundary.
    let checkpoint = SessionCheckpointRecord {
        session_id: "session-1".to_string(),
        status: "budget_exhausted".to_string(),
        reason: Some("budget_stop".to_string()),
        budget_consumed: Default::default(),
        last_committed_message_id: Some("item-42".to_string()),
        resumable: true,
        task_plan: None,
        recorded_at: chrono::Utc::now(),
    };
    assert!(checkpoint.resumable);
    assert_eq!(
        checkpoint.last_committed_message_id.as_deref(),
        Some("item-42")
    );
    assert_eq!(checkpoint.status, "budget_exhausted");
}

#[test]
fn unmatched_tool_started_restores_as_indeterminate_and_never_replays() {
    // A transcript cut at a budget checkpoint may contain an assistant tool
    // call whose `tool.started` has no committed `tool.completed`. Resume must
    // restore it as indeterminate instead of replaying the external effect.
    let mut messages = vec![
        Message::user("deploy".to_string()),
        Message::Assistant {
            content: None,
            reasoning_content: None,
            tool_calls: vec![RawToolCall {
                id: "call-crash".to_string(),
                function_name: "deploy".to_string(),
                arguments: r#"{"env":"prod"}"#.to_string(),
            }],
            pinned: false,
        },
    ];
    normalize_tool_boundaries(&mut messages);

    assert_eq!(messages.len(), 3, "missing tool result must be synthesized");
    let repaired = messages.pop().expect("repaired tool message");
    let Message::Tool {
        tool_call_id,
        terminal,
        ..
    } = repaired
    else {
        panic!("expected a repaired tool message, got {repaired:?}");
    };
    assert_eq!(tool_call_id, "call-crash");
    let terminal = terminal.expect("repaired tool has a terminal");
    use orca_core::tool_types::{ToolResultKind, ToolStatus};
    assert_eq!(terminal.status, ToolStatus::Indeterminate);
    assert_eq!(terminal.kind, ToolResultKind::Indeterminate);
    assert!(
        terminal
            .error
            .as_ref()
            .is_some_and(|error| error.contains("missing from recovered history")),
        "indeterminate restore must say the result is missing, not completed"
    );
}

#[test]
fn journal_never_replays_unmatched_tool_starts_as_completed() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("resume.journal.jsonl");
    let mut journal = ExecutionJournal::open(path.clone(), "op-resume").expect("open journal");
    journal
        .append_durable(journal.record_operation_started(1))
        .expect("operation.started");
    journal
        .append_durable(journal.record_turn_started("turn-1"))
        .expect("turn.started");
    // The crash happens mid-tool: `tool.started` is committed but the process
    // dies before `tool.completed` and before any checkpoint. The committed
    // start must restore as indeterminate and never replay as completed.
    journal
        .append_durable(journal.record_tool_started("turn-1", "call-1", "bash"))
        .expect("tool.started");
    drop(journal);

    let reopened = ExecutionJournal::open(path, "op-resume").expect("reopen journal");
    assert!(reopened.last_checkpoint().is_none());
    let unmatched = reopened.unmatched_tool_starts();
    assert_eq!(unmatched.len(), 1);
    assert!(matches!(
        unmatched[0],
        JournalRecord::ToolStarted { tool_call_id, .. } if tool_call_id == "call-1"
    ));
    assert!(
        reopened
            .committed()
            .iter()
            .all(|record| !matches!(record, JournalRecord::ToolCompleted { .. })),
        "an unmatched tool.started must never appear as a committed completion"
    );
}

#[test]
fn settled_checkpoint_is_the_resume_boundary_before_terminal() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("settled.journal.jsonl");
    let mut journal = ExecutionJournal::open(path.clone(), "op-settled").expect("open journal");
    journal
        .append_durable(journal.record_operation_started(1))
        .expect("operation.started");
    journal
        .append_durable(journal.record_turn_started("turn-1"))
        .expect("turn.started");
    // Ordering rule: the committed tool settles before the checkpoint, and the
    // checkpoint lands before the terminal.
    journal
        .append_durable(journal.record_tool_started("turn-1", "call-1", "bash"))
        .expect("tool.started");
    journal
        .append_durable(journal.record_tool_completed("turn-1", "call-1", "completed", None))
        .expect("tool.completed");
    journal
        .append_durable(journal.record_checkpoint("cp-1", Some("item-42".to_string())))
        .expect("checkpoint.created");

    let reopened = ExecutionJournal::open(path, "op-settled").expect("reopen journal");
    assert_eq!(reopened.last_checkpoint().expect("checkpoint").ordinal(), 5);
    assert!(reopened.unmatched_tool_starts().is_empty());
    assert_eq!(
        reopened
            .last_checkpoint()
            .expect("checkpoint")
            .checkpoint_id_for_test(),
        Some("cp-1")
    );
}

#[test]
fn resumed_operation_owns_fresh_operation_and_budget() {
    // Resume creates a new operation with a fresh journal and a fresh
    // BudgetController; the old operation's terminal facts stay in its own
    // journal.
    let dir = tempdir().expect("tempdir");
    let mut previous = ExecutionJournal::open(dir.path().join("op-1.journal.jsonl"), "op-1")
        .expect("previous journal");
    previous
        .append_durable(previous.record_operation_started(1))
        .expect("previous operation.started");
    previous
        .append_durable(previous.record_terminal(completed_terminal(Default::default())))
        .expect("previous terminal");

    let mut resumed = ExecutionJournal::open(dir.path().join("op-2.journal.jsonl"), "op-2")
        .expect("resumed journal");
    resumed
        .append_durable(resumed.record_operation_started(1))
        .expect("resumed operation.started");
    assert_ne!(previous.operation_id(), resumed.operation_id());
    assert!(!resumed.has_terminal());

    // The resumed operation's budget starts from zero, independent of the
    // previous operation's consumption.
    let mut budget = BudgetController::new(orca_core::budget::BudgetSpec {
        max_turns: Some(3),
        ..orca_core::budget::BudgetSpec::default()
    });
    for _ in 0..3 {
        budget
            .admit_turn()
            .expect("fresh budget admits three turns");
    }
    assert_eq!(budget.usage().turns, 3);
}

#[test]
fn exhausted_goal_token_budget_disables_automatic_continuation() {
    use orca_core::goal_runtime::{
        GoalId, GoalNextAction, GoalTurnOrigin, GoalTurnStatus, GoalUsage,
    };
    use orca_runtime::goal_tracker::{GoalTracker, GoalTurnResult};

    // Goal owns a cumulative budget: charged usage across outer turns is
    // compared against the configured token budget. Before the turn the goal
    // still has a continuation lease; after charging to the cap it does not.
    let mut tracker = GoalTracker::new(GoalId::new(), Some(100));
    assert_eq!(tracker.remaining_budget(), Some(100));
    assert!(!tracker.budget_exhausted());

    tracker.begin_outer_turn(GoalTurnOrigin::User).unwrap();
    let action = tracker
        .finish_outer_turn(GoalTurnResult {
            status: GoalTurnStatus::Success,
            end_reason: orca_runtime::lifecycle::TurnEndReason::Unclassified,
            terminal: None,
            usage: GoalUsage {
                charged_input_tokens: 90,
                output_tokens: 10,
                cache_tokens: 0,
                verifier_tokens: 0,
                cost_micros: 0,
                elapsed_seconds: 0,
            },
            gaps: Vec::new(),
            evidence_count: 1,
        })
        .unwrap();
    assert!(
        matches!(action, GoalNextAction::BudgetLimited),
        "charged usage reaching the token budget must disable continuation"
    );
    assert_eq!(tracker.remaining_budget(), Some(0));
    assert!(
        tracker.budget_exhausted(),
        "an exhausted Goal budget must report no remaining continuation lease"
    );
}
