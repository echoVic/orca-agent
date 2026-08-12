//! `OperationContext`: the operation-owned execution context bundling the
//! [`BudgetController`] (admission/accounting) with the [`ExecutionJournal`]
//! (the durable source of truth for operation facts).
//!
//! Every agent loop — the root loop and each child-agent loop — opens its own
//! context keyed by its unique turn id, journals under
//! `$ORCA_HOME/operations/<turn_id>.jsonl`, and follows the commit order
//! `tool settled → checkpoint durable → terminal durable → surface projection`:
//! budget stops commit a real `checkpoint.created` (never a fake id) and the
//! `operation.terminal` before the stop is surfaced, and every other exit
//! appends the terminal once the loop settles.

use std::io;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use orca_core::budget::{BudgetSpec, BudgetStop, OperationTerminal};

use crate::budget_controller::BudgetController;
use crate::execution_journal::{ExecutionJournal, JOURNAL_SCHEMA_VERSION, JournalRecord};
use crate::thread_store::orca_home;

/// One operation's budget controller plus its append-only journal.
pub(crate) struct OperationContext {
    pub(crate) controller: BudgetController,
    pub(crate) journal: ExecutionJournal,
}

impl OperationContext {
    /// Opens (or repairs) the operation's journal and stamps
    /// `operation.started` for a fresh journal. Reopening an existing journal
    /// (crash recovery) continues appending without a second `operation.started`.
    ///
    /// `persistent` selects the journal location: `true` journals under
    /// `$ORCA_HOME/operations` (the durable audit trail for recorded
    /// operations), `false` journals under the system temp directory so
    /// stateless operations never create runtime persistence artifacts in
    /// `ORCA_HOME`.
    pub(crate) fn open(spec: BudgetSpec, operation_id: &str, persistent: bool) -> io::Result<Self> {
        let journal = ExecutionJournal::open(journal_path(operation_id, persistent), operation_id)?;
        let mut context = Self {
            controller: BudgetController::new(spec),
            journal,
        };
        if context.journal.committed().is_empty() {
            context
                .journal
                .append_durable(JournalRecord::OperationStarted {
                    operation_id: operation_id.to_string(),
                    turn_id: String::new(),
                    ordinal: 0,
                    schema_version: JOURNAL_SCHEMA_VERSION,
                    started_at_ms: unix_ms(),
                })?;
        }
        Ok(context)
    }

    /// Reopens an existing operation journal at its exact path (the
    /// suspended-operation completion path, which must continue appending to
    /// the same file the loop used). Committed records reload; appends
    /// continue with fresh ordinals.
    pub(crate) fn reopen(
        journal_path: PathBuf,
        spec: BudgetSpec,
        operation_id: &str,
    ) -> io::Result<Self> {
        let journal = ExecutionJournal::open(journal_path, operation_id)?;
        Ok(Self {
            controller: BudgetController::new(spec),
            journal,
        })
    }

    /// Admits the model turn, then records `turn.started` durably for the
    /// admitted turn (a rejected turn never starts and leaves no record).
    /// Journal persistence faults propagate as `io::Error`; budget exhaustion
    /// returns `Err(stop)`.
    pub(crate) fn admit_turn(&mut self, turn_id: &str) -> io::Result<Result<(), BudgetStop>> {
        let admitted = self.controller.admit_turn();
        match admitted {
            Ok(()) => {
                self.journal.append_durable(JournalRecord::TurnStarted {
                    operation_id: self.journal.operation_id().to_string(),
                    turn_id: turn_id.to_string(),
                    ordinal: 0,
                    schema_version: JOURNAL_SCHEMA_VERSION,
                })?;
                Ok(Ok(()))
            }
            Err(stop) => Ok(Err(stop)),
        }
    }

    /// Admits one tool call; only an admitted call gets a durable
    /// `tool.started` (a rejected call never executes).
    pub(crate) fn admit_tool_call(
        &mut self,
        turn_id: &str,
        tool_call_id: &str,
        tool_name: &str,
    ) -> io::Result<Result<(), BudgetStop>> {
        let admitted = self.controller.admit_tool_call();
        match admitted {
            Ok(()) => {
                self.journal.append_durable(JournalRecord::ToolStarted {
                    operation_id: self.journal.operation_id().to_string(),
                    turn_id: turn_id.to_string(),
                    ordinal: 0,
                    schema_version: JOURNAL_SCHEMA_VERSION,
                    tool_call_id: tool_call_id.to_string(),
                    tool_name: tool_name.to_string(),
                })?;
                Ok(Ok(()))
            }
            Err(stop) => Ok(Err(stop)),
        }
    }

    /// Settles a tool durably. Call only after the tool's result has been
    /// committed to the conversation; a crash between `tool.started` and this
    /// record leaves the tool recoverable as `indeterminate`.
    pub(crate) fn record_tool_completed(
        &mut self,
        turn_id: &str,
        tool_call_id: &str,
        status: impl Into<String>,
        error: Option<String>,
    ) -> io::Result<()> {
        self.journal.append_durable(JournalRecord::ToolCompleted {
            operation_id: self.journal.operation_id().to_string(),
            turn_id: turn_id.to_string(),
            ordinal: 0,
            schema_version: JOURNAL_SCHEMA_VERSION,
            tool_call_id: tool_call_id.to_string(),
            status: status.into(),
            error,
        })
    }

    /// Records provider cost (USD micros spent since the last recording)
    /// against the operation. Cost facts persist with the next checkpoint or
    /// terminal's usage snapshot.
    pub(crate) fn record_cost_usd_micros(
        &mut self,
        cost_usd_micros: u64,
    ) -> Result<(), BudgetStop> {
        self.controller.record_cost_usd_micros(cost_usd_micros)
    }

    /// Publishes elapsed wall time so a long provider call stops promptly at
    /// the next accounting boundary instead of only at the next turn admit.
    pub(crate) fn sync_wall_time(&mut self) -> Result<(), BudgetStop> {
        self.controller.sync_wall_time()
    }

    /// Commits a budget stop in journal order: `checkpoint.created` durable,
    /// then `operation.terminal` durable. Returns the typed terminal carrying
    /// the real checkpoint id and resumability; the caller surfaces exactly
    /// this terminal and never reconstructs stop facts from constants. The
    /// caller already holds the `BudgetStop` that latched the controller.
    pub(crate) fn commit_budget_stop(
        &mut self,
        turn_id: &str,
        checkpoint_id: &str,
        message_id: Option<&str>,
    ) -> io::Result<OperationTerminal> {
        self.journal
            .append_durable(JournalRecord::CheckpointCreated {
                operation_id: self.journal.operation_id().to_string(),
                turn_id: turn_id.to_string(),
                ordinal: 0,
                schema_version: JOURNAL_SCHEMA_VERSION,
                checkpoint_id: checkpoint_id.to_string(),
                message_id: message_id.map(str::to_string),
            })?;
        self.controller.record_checkpoint(checkpoint_id.to_string());
        let terminal = self.controller.terminal();
        self.journal
            .append_durable(JournalRecord::OperationTerminal {
                operation_id: self.journal.operation_id().to_string(),
                turn_id: turn_id.to_string(),
                ordinal: 0,
                schema_version: JOURNAL_SCHEMA_VERSION,
                terminal: terminal.clone(),
            })?;
        Ok(terminal)
    }

    /// Commits a budget stop with the real resume boundary, in this order:
    /// the session checkpoint (carrying the durable last-committed message
    /// id and the real stop reason) is written first, then the operation
    /// journal's checkpoint and terminal. A crash after this returns can
    /// never leave a resumable terminal without a durable conversation
    /// boundary to resume from. Without a session writer (stateless
    /// operation) the stop is committed NON-resumable: no durable
    /// conversation boundary exists to resume from.
    pub(crate) fn commit_budget_stop_with_boundary(
        &mut self,
        turn_id: &str,
        events_run_id: &str,
        mut session_writer: Option<&mut crate::thread_store::SessionWriter>,
        budget_consumed: &orca_core::cost_types::UsageTotals,
        task_plan: Option<&str>,
        stop: BudgetStop,
    ) -> io::Result<OperationTerminal> {
        let checkpoint_id = format!("{events_run_id}-budget-stop");
        // 1. The session/conversation checkpoint with the real durable
        //    message boundary comes first; the resume boundary must exist
        //    before any terminal claims resumability.
        if let Some(writer) = session_writer.as_deref_mut() {
            let checkpoint = crate::thread_store::SessionCheckpointRecord {
                session_id: writer
                    .session_id()
                    .unwrap_or_else(|| events_run_id.to_string()),
                status: "budget_exhausted".to_string(),
                reason: Some(session_checkpoint_reason(&stop).to_string()),
                budget_consumed: budget_consumed.clone(),
                last_committed_message_id: writer.last_committed_message_id(),
                resumable: true,
                task_plan: task_plan.map(str::to_string),
                recorded_at: chrono::Utc::now(),
            };
            writer.append_checkpoint(checkpoint)?;
            let message_id = writer.last_committed_message_id();
            // 2. The operation journal's checkpoint references the durable
            //    boundary, then the terminal follows.
            self.commit_budget_stop(turn_id, &checkpoint_id, message_id.as_deref())
        } else {
            // Stateless: no durable conversation boundary exists, so the
            // stop must never claim resumability.
            self.commit_non_resumable_budget_stop(turn_id, &checkpoint_id)
        }
    }

    /// Commits a NON-resumable budget stop: the journal records a checkpoint
    /// with no message boundary and the Stopped terminal stays
    /// `resumable: false` (the controller never records the checkpoint, so
    /// no resume boundary is ever claimed). Used for stateless operations
    /// and suspended exchanges that exceeded their budget without a durable
    /// conversation boundary.
    pub(crate) fn commit_non_resumable_budget_stop(
        &mut self,
        turn_id: &str,
        checkpoint_id: &str,
    ) -> io::Result<OperationTerminal> {
        self.journal
            .append_durable(JournalRecord::CheckpointCreated {
                operation_id: self.journal.operation_id().to_string(),
                turn_id: turn_id.to_string(),
                ordinal: 0,
                schema_version: JOURNAL_SCHEMA_VERSION,
                checkpoint_id: checkpoint_id.to_string(),
                message_id: None,
            })?;
        // The journal fact carries the checkpoint id; the terminal keeps it
        // but NEVER claims resumability (the controller recorded no resume
        // boundary, so `resumable` stays false).
        let mut terminal = self.controller.terminal();
        if let OperationTerminal::Stopped {
            checkpoint_id: id, ..
        } = &mut terminal
        {
            *id = checkpoint_id.to_string();
        }
        self.journal
            .append_durable(JournalRecord::OperationTerminal {
                operation_id: self.journal.operation_id().to_string(),
                turn_id: turn_id.to_string(),
                ordinal: 0,
                schema_version: JOURNAL_SCHEMA_VERSION,
                terminal: terminal.clone(),
            })?;
        Ok(terminal)
    }

    /// Appends the `operation.terminal` record once, when the loop exits
    /// without an already-committed stop terminal.
    pub(crate) fn commit_terminal(
        &mut self,
        turn_id: &str,
        terminal: OperationTerminal,
    ) -> io::Result<()> {
        if self.journal.has_terminal() {
            return Ok(());
        }
        self.journal
            .append_durable(JournalRecord::OperationTerminal {
                operation_id: self.journal.operation_id().to_string(),
                turn_id: turn_id.to_string(),
                ordinal: 0,
                schema_version: JOURNAL_SCHEMA_VERSION,
                terminal,
            })
    }

    /// Test convenience: a context journaled to a unique temp path so tests
    /// never collide on the shared process-wide ORCA_HOME and never create
    /// artifacts under the per-test home.
    #[cfg(test)]
    pub(crate) fn for_tests(spec: BudgetSpec, operation_id: &str) -> Self {
        let unique = format!(
            "{operation_id}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0)
        );
        Self::open(spec, &unique, false).expect("open test operation context")
    }
}

/// The session checkpoint reason string: the legacy cost string stays
/// byte-stable for history consumers; every other dimension carries its own
/// distinct reason instead of masquerading as cost exhaustion.
fn session_checkpoint_reason(stop: &BudgetStop) -> &'static str {
    use orca_core::budget::StopReason;
    match stop.reason {
        StopReason::CostBudget { .. } => "cost_budget_exhausted",
        StopReason::TurnBudget { .. } => "turn_budget_exhausted",
        StopReason::ToolCallBudget { .. } => "tool_call_budget_exhausted",
        StopReason::WallTimeBudget { .. } => "wall_time_budget_exhausted",
    }
}

fn journal_path(operation_id: &str, persistent: bool) -> PathBuf {
    let stem = operation_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    let base = if persistent {
        orca_home().join("operations")
    } else {
        std::env::temp_dir().join("orca-operations")
    };
    base.join(format!("{stem}.jsonl"))
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_core::budget::StopReason;

    #[test]
    fn stateless_budget_stop_commits_non_resumable_terminal_without_boundary() {
        let mut context = OperationContext::for_tests(
            BudgetSpec {
                max_turns: Some(1),
                ..BudgetSpec::default()
            },
            "op-stateless",
        );
        context
            .admit_turn("turn-1")
            .expect("journal ok")
            .expect("admitted");
        let stop = context.controller.admit_turn().expect_err("exhausted");
        let budget_consumed = orca_core::cost_types::UsageTotals {
            input_tokens: 0,
            output_tokens: 0,
            cache_tokens: 0,
            estimated_cost_usd: 0.0,
        };
        let terminal = context
            .commit_budget_stop_with_boundary(
                "turn-1",
                "run-stateless",
                None,
                &budget_consumed,
                None,
                stop,
            )
            .expect("stop committed");
        match terminal {
            OperationTerminal::Stopped {
                checkpoint_id,
                resumable,
                ..
            } => {
                assert!(!resumable, "stateless stop must never claim resumability");
                assert!(
                    !checkpoint_id.is_empty(),
                    "the stop still carries its journal checkpoint id"
                );
            }
            other => panic!("expected Stopped terminal, got {other:?}"),
        }
        // The journal recorded a checkpoint with no message boundary and the
        // non-resumable terminal.
        assert!(context.journal.last_checkpoint().is_some());
        assert!(context.journal.has_terminal());
    }

    #[test]
    fn open_stamps_operation_started_once() {
        let context = OperationContext::for_tests(BudgetSpec::default(), "op-started");
        assert!(
            context
                .journal
                .committed()
                .iter()
                .any(|record| { matches!(record, JournalRecord::OperationStarted { .. }) })
        );
        assert!(context.journal.has_terminal() == false);
    }

    #[test]
    fn admit_turn_records_turn_started_after_admission() {
        let mut context = OperationContext::for_tests(BudgetSpec::default(), "op-turn");
        context
            .admit_turn("turn-1")
            .expect("journal ok")
            .expect("admitted");
        assert!(context.journal.committed().iter().any(|record| {
            matches!(record, JournalRecord::TurnStarted { turn_id, .. } if turn_id == "turn-1")
        }));
        assert_eq!(context.controller.usage().turns, 1);
    }

    #[test]
    fn tool_admission_records_started_only_when_admitted() {
        let mut context = OperationContext::for_tests(BudgetSpec::default(), "op-tool");
        context
            .admit_tool_call("turn-1", "call-1", "read_file")
            .expect("journal ok")
            .expect("admitted");
        assert!(context.journal.committed().iter().any(|record| {
            matches!(record, JournalRecord::ToolStarted { tool_call_id, .. } if tool_call_id == "call-1")
        }));
        context
            .record_tool_completed("turn-1", "call-1", "completed", None)
            .expect("settled");
        assert!(context.journal.committed().iter().any(|record| {
            matches!(record, JournalRecord::ToolCompleted { tool_call_id, .. } if tool_call_id == "call-1")
        }));
        assert!(context.journal.unmatched_tool_starts().is_empty());
    }

    #[test]
    fn exhausted_tool_admission_never_records_started() {
        let mut context = OperationContext::for_tests(
            BudgetSpec {
                max_tool_calls: Some(1),
                ..BudgetSpec::default()
            },
            "op-tool-exhausted",
        );
        context
            .admit_tool_call("turn-1", "call-1", "read_file")
            .expect("journal ok")
            .expect("admitted");
        let stopped = context
            .admit_tool_call("turn-1", "call-2", "grep")
            .expect("journal ok")
            .expect_err("second call must be stopped");
        assert!(matches!(stopped.reason, StopReason::ToolCallBudget { .. }));
        assert!(context.journal.committed().iter().all(|record| {
            !matches!(record, JournalRecord::ToolStarted { tool_call_id, .. } if tool_call_id == "call-2")
        }));
    }

    #[test]
    fn commit_budget_stop_durably_orders_checkpoint_before_terminal() {
        let mut context = OperationContext::for_tests(
            BudgetSpec {
                max_turns: Some(1),
                ..BudgetSpec::default()
            },
            "op-stop",
        );
        context
            .admit_turn("turn-1")
            .expect("journal ok")
            .expect("admitted");
        context.controller.admit_turn().expect_err("exhausted");
        let terminal = context
            .commit_budget_stop("turn-1", "checkpoint-1", Some("item-9"))
            .expect("stop committed");
        assert!(matches!(
            terminal,
            OperationTerminal::Stopped {
                resumable: true,
                ..
            }
        ));
        let kinds = context
            .journal
            .committed()
            .iter()
            .map(|record| record.kind())
            .collect::<Vec<_>>();
        let checkpoint_at = kinds
            .iter()
            .position(|kind| {
                *kind == crate::execution_journal::JournalRecordKind::CheckpointCreated
            })
            .expect("checkpoint record");
        let terminal_at = kinds
            .iter()
            .position(|kind| {
                *kind == crate::execution_journal::JournalRecordKind::OperationTerminal
            })
            .expect("terminal record");
        assert!(
            checkpoint_at < terminal_at,
            "checkpoint must precede terminal"
        );
        assert!(context.journal.has_terminal());
        let checkpoint = context
            .journal
            .last_checkpoint()
            .expect("committed checkpoint");
        let crate::execution_journal::JournalRecord::CheckpointCreated {
            checkpoint_id,
            message_id,
            ..
        } = checkpoint
        else {
            unreachable!("last_checkpoint returns checkpoint");
        };
        assert_eq!(checkpoint_id, "checkpoint-1");
        assert_eq!(message_id.as_deref(), Some("item-9"));
    }

    #[test]
    fn commit_terminal_appends_only_once() {
        let mut context = OperationContext::for_tests(BudgetSpec::default(), "op-terminal");
        context
            .commit_terminal(
                "turn-1",
                OperationTerminal::Completed {
                    usage: Default::default(),
                },
            )
            .expect("terminal committed");
        context
            .commit_terminal(
                "turn-1",
                OperationTerminal::Completed {
                    usage: Default::default(),
                },
            )
            .expect("second commit is a no-op");
        let terminals = context
            .journal
            .committed()
            .iter()
            .filter(|record| matches!(record, JournalRecord::OperationTerminal { .. }))
            .count();
        assert_eq!(terminals, 1);
    }
}
