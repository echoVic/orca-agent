//! Execution journal durability-ordering contract (Task 4).
//!
//! Proves with failure injection that:
//! - `tool.completed` is durable before `checkpoint.created`;
//! - a checkpoint is durable before `operation.terminal` is published;
//! - an unflushed `tool.started` never replays on reopen and is restored as
//!   indeterminate;
//! - committed records are the only facts projections may read.

use orca_core::budget::{BudgetSpec, BudgetUsage, OperationTerminal, StopReason};
use orca_runtime::budget_controller::BudgetController;
use orca_runtime::execution_journal::{
    ExecutionJournal, JOURNAL_SCHEMA_VERSION, JournalFaults, JournalRecord,
};
use tempfile::tempdir;

fn journal_path(dir: &tempfile::TempDir) -> std::path::PathBuf {
    dir.path().join("operation.journal.jsonl")
}

fn record_ordinals(journal: &ExecutionJournal) -> Vec<u64> {
    journal
        .committed()
        .iter()
        .map(JournalRecord::ordinal)
        .collect()
}

#[test]
fn committed_records_are_ordered_and_typed() {
    let dir = tempdir().expect("tempdir");
    let mut journal = ExecutionJournal::open(journal_path(&dir), "op-1").expect("open journal");

    journal
        .append_durable(journal.record_operation_started(1_000))
        .expect("operation.started durable");
    journal
        .append_durable(journal.record_turn_started("turn-1"))
        .expect("turn.started durable");
    journal
        .append_durable(journal.record_tool_started("turn-1", "call-1", "bash"))
        .expect("tool.started durable");
    journal
        .append_durable(journal.record_tool_completed("turn-1", "call-1", "completed", None))
        .expect("tool.completed durable");
    journal
        .append_durable(journal.record_checkpoint("cp-1", Some("msg-9".to_string())))
        .expect("checkpoint.created durable");
    journal
        .append_durable(journal.record_terminal(OperationTerminal::Completed {
            usage: BudgetUsage::default(),
        }))
        .expect("operation.terminal durable");

    let records = journal.committed();
    assert_eq!(records.len(), 6);
    assert_eq!(records[0].kind().as_str_for_test(), "operation.started");
    assert_eq!(records[1].kind().as_str_for_test(), "turn.started");
    assert_eq!(records[2].kind().as_str_for_test(), "tool.started");
    assert_eq!(records[3].kind().as_str_for_test(), "tool.completed");
    assert_eq!(records[4].kind().as_str_for_test(), "checkpoint.created");
    assert_eq!(records[5].kind().as_str_for_test(), "operation.terminal");
    // Strictly increasing ordinals; schema version on every record.
    assert_eq!(record_ordinals(&journal), vec![1, 2, 3, 4, 5, 6]);
    assert!(
        records
            .iter()
            .all(|record| record.schema_version_for_test() == JOURNAL_SCHEMA_VERSION)
    );
    assert!(records.iter().all(|record| record.operation_id() == "op-1"));
    assert!(journal.has_terminal());
    assert!(journal.unmatched_tool_starts().is_empty());
}

#[test]
fn reopen_loads_only_committed_records_in_order() {
    let dir = tempdir().expect("tempdir");
    let path = journal_path(&dir);
    {
        let mut journal = ExecutionJournal::open(path.clone(), "op-reopen").expect("open journal");
        journal
            .append_durable(journal.record_operation_started(1))
            .expect("operation.started");
        journal
            .append_durable(journal.record_turn_started("turn-1"))
            .expect("turn.started");
        journal
            .append_durable(journal.record_checkpoint("cp-1", None))
            .expect("checkpoint.created");
    }
    let reopened = ExecutionJournal::open(path, "op-reopen").expect("reopen journal");
    assert_eq!(reopened.committed().len(), 3);
    assert_eq!(
        reopened
            .committed()
            .iter()
            .map(JournalRecord::kind)
            .map(|kind| kind.as_str_for_test())
            .collect::<Vec<_>>(),
        ["operation.started", "turn.started", "checkpoint.created"]
    );
    // Ordinals continue after the last committed record.
    let checkpoint = reopened.last_checkpoint().expect("checkpoint present");
    assert_eq!(checkpoint.ordinal(), 3);
}

#[test]
fn unflushed_tool_started_never_replays_on_reopen() {
    let dir = tempdir().expect("tempdir");
    let path = journal_path(&dir);
    {
        let mut journal = ExecutionJournal::open(path.clone(), "op-crash").expect("open journal");
        journal
            .append_durable(journal.record_operation_started(1))
            .expect("operation.started");
        journal
            .append_durable(journal.record_turn_started("turn-1"))
            .expect("turn.started");
        // A tool starts but the journal never flushes its completion — the
        // simulated crash drops `pending` entirely.
        journal
            .append(journal.record_tool_started("turn-1", "call-crash", "bash"))
            .expect("tool.started appended (pending)");
        assert_eq!(journal.pending().len(), 1);
        // Drop without flush: the tool.started is lost with the crash.
    }
    let reopened = ExecutionJournal::open(path, "op-crash").expect("reopen journal");
    assert_eq!(reopened.committed().len(), 2);
    assert!(
        reopened
            .committed()
            .iter()
            .all(|record| !matches!(record, JournalRecord::ToolStarted { .. })),
        "an uncommitted tool.started must never replay as a committed fact"
    );
    // Restore semantics: the caller turns unmatched starts into indeterminate
    // results; nothing is replayed as completed.
    assert!(reopened.unmatched_tool_starts().is_empty());
}

#[test]
fn flush_failure_keeps_pending_records_out_of_committed() {
    let dir = tempdir().expect("tempdir");
    let mut journal = ExecutionJournal::open(journal_path(&dir), "op-fault").expect("open journal");
    journal
        .append_durable(journal.record_operation_started(1))
        .expect("operation.started durable");
    journal.set_faults(JournalFaults {
        fail_next_flush: true,
        ..JournalFaults::default()
    });
    journal
        .append(journal.record_turn_started("turn-1"))
        .expect("turn.started appended");

    // The armed fault makes this flush fail before writing; the record stays
    // pending and must not be observable as committed.
    let error = journal.flush().expect_err("injected flush failure");
    assert!(error.to_string().contains("injected flush failure"));
    assert!(
        journal
            .committed()
            .iter()
            .all(|record| { !matches!(record, JournalRecord::TurnStarted { .. }) })
    );
    assert_eq!(journal.pending().len(), 1);

    // A later flush without the fault succeeds atomically.
    journal.flush().expect("flush succeeds after fault cleared");
    assert_eq!(journal.committed().len(), 2);
    assert!(journal.pending().is_empty());
}

#[test]
fn checkpoint_requires_settled_open_tools() {
    let dir = tempdir().expect("tempdir");
    let mut journal = ExecutionJournal::open(journal_path(&dir), "op-open-tool").expect("open");
    journal
        .append_durable(journal.record_operation_started(1))
        .expect("operation.started");
    journal
        .append_durable(journal.record_turn_started("turn-1"))
        .expect("turn.started");
    journal
        .append_durable(journal.record_tool_started("turn-1", "call-1", "bash"))
        .expect("tool.started");

    let error = journal
        .append(journal.record_checkpoint("cp-1", None))
        .expect_err("checkpoint rejected while a tool is open");
    assert!(error.contains("tool.started"));
    assert!(!journal.has_terminal());

    // Settling the tool then checkpointing is allowed.
    journal
        .append_durable(journal.record_tool_completed("turn-1", "call-1", "completed", None))
        .expect("tool.completed");
    journal
        .append_durable(journal.record_checkpoint("cp-1", Some("msg-4".to_string())))
        .expect("checkpoint.created after settlement");
}

#[test]
fn stopped_terminal_requires_committed_checkpoint() {
    let dir = tempdir().expect("tempdir");
    let mut journal = ExecutionJournal::open(journal_path(&dir), "op-stop").expect("open");
    journal
        .append_durable(journal.record_operation_started(1))
        .expect("operation.started");
    journal
        .append_durable(journal.record_turn_started("turn-1"))
        .expect("turn.started");

    let stopped = OperationTerminal::Stopped {
        reason: StopReason::TurnBudget { max_turns: 3 },
        usage: BudgetUsage {
            turns: 3,
            tool_calls: 0,
            cost_usd_micros: 0,
            wall_time_ms: 0,
        },
        checkpoint_id: String::new(),
        resumable: false,
    };
    let error = journal
        .append(journal.record_terminal(stopped.clone()))
        .expect_err("stopped terminal without checkpoint rejected");
    assert!(error.contains("checkpoint.created"));

    // A committed checkpoint flips the ordering: checkpoint durable before
    // terminal publication.
    journal
        .append_durable(journal.record_checkpoint("cp-boundary", Some("msg-9".to_string())))
        .expect("checkpoint.created durable");
    journal
        .append_durable(journal.record_terminal(stopped))
        .expect("stopped terminal after committed checkpoint");
    assert!(journal.has_terminal());
    assert!(matches!(
        journal.terminal(),
        Some(OperationTerminal::Stopped {
            resumable: false,
            ..
        })
    ));
}

#[test]
fn terminal_may_be_appended_only_once() {
    let dir = tempdir().expect("tempdir");
    let mut journal = ExecutionJournal::open(journal_path(&dir), "op-once").expect("open");
    journal
        .append_durable(journal.record_operation_started(1))
        .expect("operation.started");
    journal
        .append_durable(journal.record_terminal(OperationTerminal::Completed {
            usage: BudgetUsage::default(),
        }))
        .expect("first terminal");

    let error = journal
        .append(journal.record_terminal(OperationTerminal::Completed {
            usage: BudgetUsage::default(),
        }))
        .expect_err("second terminal rejected");
    assert!(error.contains("only once"));
}

#[test]
fn checkpoint_before_terminal_survives_reopen_and_projection() {
    let dir = tempdir().expect("tempdir");
    let path = journal_path(&dir);
    let mut controller = BudgetController::new(BudgetSpec {
        max_turns: Some(3),
        ..BudgetSpec::default()
    });
    for _ in 0..3 {
        controller.admit_turn().expect("turn");
    }
    controller.admit_turn().expect_err("stopped");

    let usage = controller.usage();
    let mut journal = ExecutionJournal::open(path.clone(), "op-projection").expect("open");
    journal
        .append_durable(journal.record_operation_started(1))
        .expect("operation.started");
    journal
        .append_durable(journal.record_turn_started("turn-1"))
        .expect("turn.started");
    journal
        .append_durable(journal.record_checkpoint("cp-durable", Some("msg-7".to_string())))
        .expect("checkpoint durable");
    let terminal = OperationTerminal::Stopped {
        reason: StopReason::TurnBudget { max_turns: 3 },
        usage,
        checkpoint_id: "cp-durable".to_string(),
        resumable: true,
    };
    journal
        .append_durable(journal.record_terminal(terminal))
        .expect("terminal durable after checkpoint");

    // The projection is derived from committed records only.
    let mut projection = Vec::new();
    journal
        .write_projection(&mut projection)
        .expect("projection writes");
    let lines = String::from_utf8(projection).expect("utf8 projection");
    let line_count = lines.lines().count();
    assert_eq!(line_count, 4);

    let reopened = ExecutionJournal::open(path, "op-projection").expect("reopen");
    assert_eq!(reopened.committed().len(), 4);
    let terminal = reopened.terminal().expect("terminal restored");
    assert!(matches!(
        terminal,
        OperationTerminal::Stopped {
            checkpoint_id,
            resumable: true,
            ..
        } if checkpoint_id == "cp-durable"
    ));
}

#[test]
fn reopen_recovers_from_torn_final_line_mid_write() {
    let dir = tempdir().expect("tempdir");
    let path = journal_path(&dir);
    {
        let mut journal = ExecutionJournal::open(path.clone(), "op-torn").expect("open journal");
        journal
            .append_durable(journal.record_operation_started(1))
            .expect("operation.started");
        journal
            .append_durable(journal.record_turn_started("turn-1"))
            .expect("turn.started");
        journal
            .append_durable(journal.record_tool_started("turn-1", "call-1", "bash"))
            .expect("tool.started");
    }
    // Simulate a crash mid-write: append a partial line with no trailing
    // newline (a torn serialization of the next record).
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open journal for tearing");
    use std::io::Write;
    file.write_all(br#"{"type":"tool.completed","operation_id":"op-torn","turn_id":"turn-1","ordinal":4,"schema_version":1,"tool_call_id":"call-1","status":"comp"#)
        .expect("write torn line");
    file.flush().expect("flush torn line");
    drop(file);

    // Reopen must repair the torn tail and load only complete records.
    let reopened = ExecutionJournal::open(path, "op-torn").expect("reopen after torn write");
    assert_eq!(reopened.committed().len(), 3);
    assert!(
        reopened
            .committed()
            .iter()
            .all(|record| { !matches!(record, JournalRecord::ToolCompleted { .. }) })
    );
    // The unmatched tool start is preserved for indeterminate restore.
    assert_eq!(reopened.unmatched_tool_starts().len(), 1);
    // The next append continues after the repaired boundary. The open tool
    // settles first (ordering rule), then the checkpoint lands on the next
    // ordinal.
    let mut journal = reopened;
    journal
        .append_durable(journal.record_tool_completed("turn-1", "call-1", "completed", None))
        .expect("tool completed after repair");
    journal
        .append_durable(journal.record_checkpoint("cp-1", Some("item-3".to_string())))
        .expect("checkpoint after repair");
    assert_eq!(journal.committed().len(), 5);
    assert_eq!(journal.committed()[3].ordinal(), 4);
    assert_eq!(journal.committed()[4].ordinal(), 5);
}

#[test]
fn append_stamps_unique_ordinals_for_prebuilt_batches() {
    let dir = tempdir().expect("tempdir");
    let mut journal = ExecutionJournal::open(journal_path(&dir), "op-batch").expect("open journal");

    // Build a batch of records before appending any of them; each append must
    // receive a distinct ordinal even though the builders ran first.
    let started = journal.record_operation_started(1);
    let turn = journal.record_turn_started("turn-1");
    let tool = journal.record_tool_started("turn-1", "call-1", "bash");

    journal.append(started).expect("operation.started");
    journal.append(turn).expect("turn.started");
    journal.append(tool).expect("tool.started");
    journal.flush().expect("flush batch");

    let ordinals = record_ordinals(&journal);
    assert_eq!(ordinals, vec![1, 2, 3]);
    assert_eq!(
        ordinals
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        3,
        "prebuilt batch records must not share ordinals"
    );
}
