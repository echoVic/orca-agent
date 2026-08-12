//! `BudgetController`: one controller owns all operation limits.
//!
//! Admission, accounting, soft-landing reminders, and child leases live here;
//! the agent loop and adapters never reconstruct limits from constants. Usage
//! counters saturate, reminders never mutate usage or success state, and a
//! `Stopped` terminal is only resumable after a checkpoint is recorded.

use std::time::Instant;

use orca_core::budget::{BudgetSpec, BudgetStop, BudgetUsage, OperationTerminal, StopReason};

/// How the controller ended; `terminal()` projects this into the typed
/// [`OperationTerminal`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalState {
    Running,
    Stopped(StopReason),
}

pub struct BudgetController {
    spec: BudgetSpec,
    usage: BudgetUsage,
    started_at: Instant,
    state: TerminalState,
    checkpoint_id: Option<String>,
    inner_turn_reminder_index: u32,
    cost_reminder_index: u32,
    pending_soft_landing: Option<String>,
}

impl BudgetController {
    pub fn new(spec: BudgetSpec) -> Self {
        spec.validate()
            .expect("budget spec must validate before controller construction");
        Self {
            spec,
            usage: BudgetUsage::default(),
            started_at: Instant::now(),
            state: TerminalState::Running,
            checkpoint_id: None,
            inner_turn_reminder_index: 0,
            cost_reminder_index: 0,
            pending_soft_landing: None,
        }
    }

    pub fn spec(&self) -> &BudgetSpec {
        &self.spec
    }

    pub fn usage(&self) -> BudgetUsage {
        self.usage
    }

    pub fn is_unlimited(&self) -> bool {
        self.spec.is_unlimited()
    }

    /// Admit one model turn. On success the turn counter is incremented; on
    /// the first exhausted dimension a typed [`BudgetStop`] is returned and
    /// the controller latches the stop.
    pub fn admit_turn(&mut self) -> Result<(), BudgetStop> {
        self.sync_wall_time();
        if let Some(stop) = self.stop_if_exhausted() {
            return Err(stop);
        }
        self.usage.add_turn();
        self.observe_inner_turn_soft_landing();
        Ok(())
    }

    /// Admit one tool call within the current turn.
    pub fn admit_tool_call(&mut self) -> Result<(), BudgetStop> {
        self.sync_wall_time();
        if let Some(stop) = self.stop_if_exhausted() {
            return Err(stop);
        }
        self.usage.add_tool_call();
        Ok(())
    }

    /// Record provider cost (USD micros) spent so far.
    pub fn record_cost_usd_micros(&mut self, cost_usd_micros: u64) -> Result<(), BudgetStop> {
        self.sync_wall_time();
        if let Some(stop) = self.stop_if_exhausted() {
            return Err(stop);
        }
        self.usage.add_cost_usd_micros(cost_usd_micros);
        self.observe_cost_soft_landing();
        self.stop_if_exhausted().map_or(Ok(()), Err)
    }

    /// Merge a child lease's consumed usage into this operation.
    pub fn merge_child_usage(&mut self, child_usage: BudgetUsage) -> Result<(), BudgetStop> {
        self.sync_wall_time();
        if let Some(stop) = self.stop_if_exhausted() {
            return Err(stop);
        }
        self.usage.merge(child_usage);
        self.observe_cost_soft_landing();
        self.stop_if_exhausted().map_or(Ok(()), Err)
    }

    /// Record a committed conversation boundary. `resumable` becomes true only
    /// after this exists; the terminal is published only with it.
    pub fn record_checkpoint(&mut self, checkpoint_id: impl Into<String>) {
        self.checkpoint_id = Some(checkpoint_id.into());
    }

    /// Soft-landing reminder produced by the latest accounting update,
    /// consumed by the next model-turn opening. Never mutates usage or
    /// success state beyond the delivery index.
    pub fn take_pending_soft_landing(&mut self) -> Option<String> {
        self.pending_soft_landing.take()
    }

    /// Reserve a child budget from this operation's remaining capacity. The
    /// child's effective spec is the intersection of its own limits with what
    /// the parent has left, so a child can never spend beyond the parent's
    /// operation. Consumed usage reports back via [`BudgetLease::finish`].
    pub fn child_lease(&mut self, child_spec: BudgetSpec) -> Result<BudgetLease, BudgetStop> {
        child_spec
            .validate()
            .expect("child budget spec must validate");
        self.sync_wall_time();
        if let Some(stop) = self.stop_if_exhausted() {
            return Err(stop);
        }
        Ok(BudgetLease::new(intersect_specs(
            self.remaining_spec(),
            child_spec,
        )))
    }

    /// The typed operation terminal. `Completed` is returned while running;
    /// after a stop it is `Stopped` with the recorded checkpoint (empty and
    /// non-resumable when no checkpoint exists yet).
    pub fn terminal(&self) -> OperationTerminal {
        match self.state {
            TerminalState::Running => OperationTerminal::Completed { usage: self.usage },
            TerminalState::Stopped(reason) => OperationTerminal::Stopped {
                reason,
                usage: self.usage,
                checkpoint_id: self.checkpoint_id.clone().unwrap_or_default(),
                resumable: self.checkpoint_id.is_some(),
            },
        }
    }

    /// What this operation still has left, per dimension.
    fn remaining_spec(&self) -> BudgetSpec {
        let subtract_turns =
            |limit: Option<u32>| limit.map(|limit| limit.saturating_sub(self.usage.turns).max(1));
        let subtract_tools = |limit: Option<u32>| {
            limit.map(|limit| limit.saturating_sub(self.usage.tool_calls).max(1))
        };
        BudgetSpec {
            max_turns: subtract_turns(self.spec.max_turns),
            max_tool_calls: subtract_tools(self.spec.max_tool_calls),
            max_cost_usd_micros: self
                .spec
                .max_cost_usd_micros
                .map(|limit| limit.saturating_sub(self.usage.cost_usd_micros).max(1)),
            max_wall_time_ms: self
                .spec
                .max_wall_time_ms
                .map(|limit| limit.saturating_sub(self.usage.wall_time_ms).max(1)),
        }
    }

    fn sync_wall_time(&mut self) {
        let elapsed_ms = self.started_at.elapsed().as_millis() as u64;
        if elapsed_ms > self.usage.wall_time_ms {
            self.usage.wall_time_ms = elapsed_ms;
        }
    }

    fn stop_if_exhausted(&mut self) -> Option<BudgetStop> {
        let reason = match self.state {
            TerminalState::Stopped(reason) => reason,
            TerminalState::Running => self.exhausted_dimension()?,
        };
        self.state = TerminalState::Stopped(reason);
        Some(BudgetStop {
            reason,
            usage: self.usage,
        })
    }

    fn exhausted_dimension(&self) -> Option<StopReason> {
        if let Some(max_turns) = self.spec.max_turns
            && self.usage.turns >= max_turns
        {
            return Some(StopReason::TurnBudget { max_turns });
        }
        if let Some(max_tool_calls) = self.spec.max_tool_calls
            && self.usage.tool_calls >= max_tool_calls
        {
            return Some(StopReason::ToolCallBudget { max_tool_calls });
        }
        if let Some(max_cost_usd_micros) = self.spec.max_cost_usd_micros
            && self.usage.cost_usd_micros > max_cost_usd_micros
        {
            return Some(StopReason::CostBudget {
                max_cost_usd_micros,
            });
        }
        if let Some(max_wall_time_ms) = self.spec.max_wall_time_ms
            && self.usage.wall_time_ms > max_wall_time_ms
        {
            return Some(StopReason::WallTimeBudget { max_wall_time_ms });
        }
        None
    }

    fn observe_inner_turn_soft_landing(&mut self) {
        let Some(max_turns) = self.spec.max_turns else {
            return;
        };
        let Some(reminder) = crate::budget_soft_landing::pending_inner_turn_reminder(
            max_turns,
            self.usage.turns,
            self.inner_turn_reminder_index,
        ) else {
            return;
        };
        self.inner_turn_reminder_index = reminder.reminder_index;
        self.pending_soft_landing = Some(crate::budget_soft_landing::format_soft_landing_message(
            &reminder,
        ));
    }

    fn observe_cost_soft_landing(&mut self) {
        let Some(max_cost_usd_micros) = self.spec.max_cost_usd_micros else {
            return;
        };
        let Some(reminder) = crate::budget_soft_landing::pending_cost_budget_reminder(
            max_cost_usd_micros,
            self.usage.cost_usd_micros,
            self.cost_reminder_index,
        ) else {
            return;
        };
        self.cost_reminder_index = reminder.reminder_index;
        self.pending_soft_landing = Some(crate::budget_soft_landing::format_soft_landing_message(
            &reminder,
        ));
    }
}

/// A reserved slice of the parent operation's budget, passed into child
/// contexts. Consumed usage reports upward via [`BudgetLease::finish`].
#[derive(Debug)]
pub struct BudgetLease {
    effective_spec: BudgetSpec,
    usage: BudgetUsage,
}

impl BudgetLease {
    fn new(effective_spec: BudgetSpec) -> Self {
        Self {
            effective_spec,
            usage: BudgetUsage::default(),
        }
    }

    pub fn spec(&self) -> &BudgetSpec {
        &self.effective_spec
    }

    pub fn usage(&self) -> BudgetUsage {
        self.usage
    }

    pub fn admit_turn(&mut self) -> Result<(), BudgetStop> {
        if let Some(stop) = self.stop_if_exhausted() {
            return Err(stop);
        }
        self.usage.add_turn();
        Ok(())
    }

    pub fn admit_tool_call(&mut self) -> Result<(), BudgetStop> {
        if let Some(stop) = self.stop_if_exhausted() {
            return Err(stop);
        }
        self.usage.add_tool_call();
        Ok(())
    }

    pub fn record_cost_usd_micros(&mut self, cost_usd_micros: u64) -> Result<(), BudgetStop> {
        if let Some(stop) = self.stop_if_exhausted() {
            return Err(stop);
        }
        self.usage.add_cost_usd_micros(cost_usd_micros);
        self.stop_if_exhausted().map_or(Ok(()), Err)
    }

    /// Consumed usage receipt for the parent to merge. Unused reservation
    /// capacity is implicitly returned: only what was consumed is reported.
    pub fn finish(self) -> BudgetUsage {
        self.usage
    }

    fn stop_if_exhausted(&self) -> Option<BudgetStop> {
        let usage = self.usage;
        if let Some(max_turns) = self.effective_spec.max_turns
            && usage.turns >= max_turns
        {
            return Some(BudgetStop {
                reason: StopReason::TurnBudget { max_turns },
                usage,
            });
        }
        if let Some(max_tool_calls) = self.effective_spec.max_tool_calls
            && usage.tool_calls >= max_tool_calls
        {
            return Some(BudgetStop {
                reason: StopReason::ToolCallBudget { max_tool_calls },
                usage,
            });
        }
        if let Some(max_cost_usd_micros) = self.effective_spec.max_cost_usd_micros
            && usage.cost_usd_micros > max_cost_usd_micros
        {
            return Some(BudgetStop {
                reason: StopReason::CostBudget {
                    max_cost_usd_micros,
                },
                usage,
            });
        }
        if let Some(max_wall_time_ms) = self.effective_spec.max_wall_time_ms
            && usage.wall_time_ms > max_wall_time_ms
        {
            return Some(BudgetStop {
                reason: StopReason::WallTimeBudget { max_wall_time_ms },
                usage,
            });
        }
        None
    }
}

/// Per-dimension intersection of two specs: the tightest bound of both.
fn intersect_specs(left: BudgetSpec, right: BudgetSpec) -> BudgetSpec {
    fn min_u32(left: Option<u32>, right: Option<u32>) -> Option<u32> {
        match (left, right) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (Some(value), None) | (None, Some(value)) => Some(value),
            (None, None) => None,
        }
    }
    fn min_u64(left: Option<u64>, right: Option<u64>) -> Option<u64> {
        match (left, right) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (Some(value), None) | (None, Some(value)) => Some(value),
            (None, None) => None,
        }
    }
    BudgetSpec {
        max_turns: min_u32(left.max_turns, right.max_turns),
        max_tool_calls: min_u32(left.max_tool_calls, right.max_tool_calls),
        max_cost_usd_micros: min_u64(left.max_cost_usd_micros, right.max_cost_usd_micros),
        max_wall_time_ms: min_u64(left.max_wall_time_ms, right.max_wall_time_ms),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> BudgetSpec {
        BudgetSpec::default()
    }

    #[test]
    fn unlimited_controller_admits_more_than_128_turns_and_tools() {
        let mut controller = BudgetController::new(spec());
        for _ in 0..200 {
            controller.admit_turn().expect("unlimited turns admit");
            controller
                .admit_tool_call()
                .expect("unlimited tool calls admit");
        }
        assert_eq!(controller.usage().turns, 200);
        assert_eq!(controller.usage().tool_calls, 200);
        assert!(matches!(
            controller.terminal(),
            OperationTerminal::Completed { usage }
                if usage.turns == 200 && usage.tool_calls == 200
        ));
    }

    #[test]
    fn turn_budget_stops_on_exhausted_dimension() {
        let mut controller = BudgetController::new(BudgetSpec {
            max_turns: Some(3),
            ..spec()
        });
        for _ in 0..3 {
            controller.admit_turn().expect("within budget");
        }
        let stop = controller.admit_turn().expect_err("4th turn stopped");
        assert_eq!(stop.reason, StopReason::TurnBudget { max_turns: 3 });
        assert_eq!(stop.usage.turns, 3);
        // The stop latches: further admissions keep returning the same reason.
        let again = controller.admit_turn().expect_err("latched stop");
        assert_eq!(again.reason, stop.reason);
    }

    #[test]
    fn tool_call_budget_stops_independently_of_turns() {
        let mut controller = BudgetController::new(BudgetSpec {
            max_tool_calls: Some(2),
            ..spec()
        });
        controller.admit_turn().expect("turn admits");
        controller.admit_tool_call().expect("tool 1");
        controller.admit_tool_call().expect("tool 2");
        let stop = controller.admit_tool_call().expect_err("tool 3 stopped");
        assert_eq!(
            stop.reason,
            StopReason::ToolCallBudget { max_tool_calls: 2 }
        );
        assert_eq!(stop.usage.tool_calls, 2);
    }

    #[test]
    fn cost_budget_stops_when_usage_exceeds_limit() {
        let mut controller = BudgetController::new(BudgetSpec {
            max_cost_usd_micros: Some(1_000),
            ..spec()
        });
        controller
            .record_cost_usd_micros(900)
            .expect("under budget");
        let stop = controller
            .record_cost_usd_micros(200)
            .expect_err("over budget");
        assert_eq!(
            stop.reason,
            StopReason::CostBudget {
                max_cost_usd_micros: 1_000
            }
        );
        assert_eq!(stop.usage.cost_usd_micros, 1_100);
    }

    #[test]
    fn wall_time_budget_stops_when_elapsed_exceeds_limit() {
        let mut controller = BudgetController::new(BudgetSpec {
            max_wall_time_ms: Some(1),
            ..spec()
        });
        std::thread::sleep(std::time::Duration::from_millis(2));
        let stop = controller.admit_turn().expect_err("wall time exceeded");
        assert!(matches!(stop.reason, StopReason::WallTimeBudget { .. }));
    }

    #[test]
    fn checkpoint_controls_resumability_of_stopped_terminal() {
        let mut controller = BudgetController::new(BudgetSpec {
            max_turns: Some(1),
            ..spec()
        });
        controller.admit_turn().expect("turn 1");
        controller.admit_turn().expect_err("turn 2 stopped");

        let before = controller.terminal();
        match before {
            OperationTerminal::Stopped {
                checkpoint_id,
                resumable,
                ..
            } => {
                assert!(!resumable);
                assert!(checkpoint_id.is_empty());
            }
            other => panic!("expected Stopped, got {other:?}"),
        }

        controller.record_checkpoint("cp-1");
        match controller.terminal() {
            OperationTerminal::Stopped {
                checkpoint_id,
                resumable,
                ..
            } => {
                assert!(resumable);
                assert_eq!(checkpoint_id, "cp-1");
            }
            other => panic!("expected Stopped, got {other:?}"),
        }
    }

    #[test]
    fn inner_turn_soft_landing_reminders_arrive_before_the_wall() {
        let mut controller = BudgetController::new(BudgetSpec {
            max_turns: Some(32),
            ..spec()
        });
        assert!(controller.take_pending_soft_landing().is_none());
        // 32 - 16 = 16 remaining → first threshold.
        for _ in 0..16 {
            controller.admit_turn().expect("admit");
        }
        let reminder = controller
            .take_pending_soft_landing()
            .expect("16-remaining reminder");
        assert!(reminder.contains("16"));
        // Delivered once per threshold.
        assert!(controller.take_pending_soft_landing().is_none());
        // Usage and terminal state are unaffected by reminders.
        assert!(matches!(
            controller.terminal(),
            OperationTerminal::Completed { .. }
        ));
    }

    #[test]
    fn cost_soft_landing_reminder_uses_fraction_thresholds() {
        let mut controller = BudgetController::new(BudgetSpec {
            max_cost_usd_micros: Some(1_000_000),
            ..spec()
        });
        assert!(controller.take_pending_soft_landing().is_none());
        // 25% remaining.
        controller
            .record_cost_usd_micros(750_000)
            .expect("under budget");
        let reminder = controller
            .take_pending_soft_landing()
            .expect("25%-remaining reminder");
        assert!(reminder.contains("Cost budget"));
        assert!(controller.take_pending_soft_landing().is_none());
    }

    #[test]
    fn child_lease_is_bounded_by_child_spec_and_reports_consumed_usage() {
        let mut controller = BudgetController::new(spec());
        let mut lease = controller
            .child_lease(BudgetSpec {
                max_turns: Some(2),
                ..spec()
            })
            .expect("child lease granted");
        lease.admit_turn().expect("child turn 1");
        lease.admit_turn().expect("child turn 2");
        let stop = lease.admit_turn().expect_err("child turn 3 stopped");
        assert_eq!(stop.reason, StopReason::TurnBudget { max_turns: 2 });

        let consumed = lease.finish();
        assert_eq!(consumed.turns, 2);

        // The parent operation merges the child's consumed usage.
        controller
            .merge_child_usage(consumed)
            .expect("child usage merges");
        assert_eq!(controller.usage().turns, 2);
    }

    #[test]
    fn child_lease_never_exceeds_parent_remaining_budget() {
        let mut controller = BudgetController::new(BudgetSpec {
            max_turns: Some(1),
            ..spec()
        });
        controller.admit_turn().expect("parent turn 1");
        // Parent has no remaining turns; the lease grant itself is rejected.
        let stop = controller
            .child_lease(BudgetSpec {
                max_turns: Some(1),
                ..spec()
            })
            .expect_err("parent exhausted");
        assert_eq!(stop.reason, StopReason::TurnBudget { max_turns: 1 });
    }

    #[test]
    fn child_lease_effective_spec_is_intersection_with_parent_remaining() {
        let mut controller = BudgetController::new(BudgetSpec {
            max_turns: Some(3),
            ..spec()
        });
        controller.admit_turn().expect("parent turn 1");
        let mut lease = controller
            .child_lease(BudgetSpec {
                max_turns: Some(3),
                ..spec()
            })
            .expect("child lease granted");
        // Parent has 2 turns left; the child is capped at 2 even though its
        // own spec asked for 3.
        lease.admit_turn().expect("child turn 1");
        lease.admit_turn().expect("child turn 2");
        let stop = lease.admit_turn().expect_err("child turn 3 stopped");
        assert_eq!(stop.reason, StopReason::TurnBudget { max_turns: 2 });
    }

    #[test]
    fn zero_dimensions_are_rejected_at_construction() {
        let result = std::panic::catch_unwind(|| {
            BudgetController::new(BudgetSpec {
                max_turns: Some(0),
                ..spec()
            })
        });
        assert!(result.is_err());
    }
}
