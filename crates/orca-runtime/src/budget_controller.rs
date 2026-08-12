//! `BudgetController`: one controller owns all operation limits.
//!
//! Admission, accounting, soft-landing reminders, and child leases live here;
//! the agent loop and adapters never reconstruct limits from constants. Usage
//! counters saturate, reminders never mutate usage or success state, and a
//! `Stopped` terminal is only resumable after a checkpoint is recorded.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use orca_core::budget::{BudgetSpec, BudgetStop, BudgetUsage, OperationTerminal, StopReason};

/// How the controller ended; `terminal()` projects this into the typed
/// [`OperationTerminal`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalState {
    Running,
    Stopped(StopReason),
}

/// Capacity reserved by outstanding child leases. A lease deducts its
/// effective ceiling here when granted and returns it when settled (consumed
/// usage reports upward separately via `merge_child_usage`), so concurrent
/// children can never double-spend the parent's remaining budget.
#[derive(Debug, Default)]
pub struct LeaseReservationPool {
    reserved: BudgetUsage,
}

/// The pool shared by a controller and every lease it granted.
type SharedReservations = Arc<Mutex<LeaseReservationPool>>;

pub struct BudgetController {
    spec: BudgetSpec,
    usage: BudgetUsage,
    started_at: Instant,
    state: TerminalState,
    checkpoint_id: Option<String>,
    inner_turn_reminder_index: u32,
    cost_reminder_index: u32,
    pending_soft_landing: Option<String>,
    reservations: SharedReservations,
}

impl BudgetController {
    /// Constructs a controller for the given spec. Invalid dimensions (zero or
    /// otherwise non-positive) are treated as unlimited instead of panicking:
    /// config decoding validates and rejects bad values with a clear error
    /// before the runtime worker runs, and this constructor is a last-resort
    /// safety net that must never crash the worker.
    pub fn new(spec: BudgetSpec) -> Self {
        let spec = BudgetSpec {
            max_turns: spec.max_turns.filter(|value| *value > 0),
            max_tool_calls: spec.max_tool_calls.filter(|value| *value > 0),
            max_cost_usd_micros: spec.max_cost_usd_micros.filter(|value| *value > 0),
            max_wall_time_ms: spec.max_wall_time_ms.filter(|value| *value > 0),
        };
        Self {
            spec,
            usage: BudgetUsage::default(),
            started_at: Instant::now(),
            state: TerminalState::Running,
            checkpoint_id: None,
            inner_turn_reminder_index: 0,
            cost_reminder_index: 0,
            pending_soft_landing: None,
            reservations: Arc::new(Mutex::new(LeaseReservationPool::default())),
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
        self.sync_wall_time_inner();
        if let Some(stop) = self.stop_if_exhausted() {
            return Err(stop);
        }
        self.usage.add_turn();
        self.observe_inner_turn_soft_landing();
        Ok(())
    }

    /// Admit one tool call within the current turn. The turn dimension is not
    /// a tool constraint: a turn already admitted by [`admit_turn`] may keep
    /// using tools when `max_turns` is fully consumed (the stop lands at the
    /// next turn admit instead).
    pub fn admit_tool_call(&mut self) -> Result<(), BudgetStop> {
        self.sync_wall_time_inner();
        if let Some(stop) = self.stop_if_exhausted_without_turn_dimension() {
            return Err(stop);
        }
        self.usage.add_tool_call();
        Ok(())
    }

    /// Record provider cost (USD micros) spent so far.
    pub fn record_cost_usd_micros(&mut self, cost_usd_micros: u64) -> Result<(), BudgetStop> {
        self.sync_wall_time_inner();
        if let Some(stop) = self.stop_if_exhausted_without_turn_dimension() {
            return Err(stop);
        }
        self.usage.add_cost_usd_micros(cost_usd_micros);
        self.observe_cost_soft_landing();
        self.stop_if_exhausted_without_turn_dimension()
            .map_or(Ok(()), Err)
    }

    /// Merge a child lease's consumed usage into this operation.
    pub fn merge_child_usage(&mut self, child_usage: BudgetUsage) -> Result<(), BudgetStop> {
        self.sync_wall_time_inner();
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
    /// the parent has left *after* deducting every outstanding reservation,
    /// so concurrent children can never double-spend the parent's operation.
    /// A dimension with nothing left (consumed or reserved) grants no
    /// capacity: the lease is refused instead of padding the remainder, so a
    /// child can never be handed budget the parent does not have.
    /// The reservation is held for the lease's lifetime and returned when the
    /// lease settles (RAII via [`BudgetLease::finish`] and `Drop`); consumed
    /// usage reports back through [`BudgetController::merge_child_usage`].
    pub fn child_lease(&mut self, child_spec: BudgetSpec) -> Result<BudgetLease, BudgetStop> {
        child_spec
            .validate()
            .expect("child budget spec must validate");
        self.sync_wall_time_inner();
        if let Some(stop) = self.stop_if_exhausted() {
            return Err(stop);
        }
        let reserved = self
            .reservations
            .lock()
            .expect("lease reservation pool")
            .reserved;
        let remaining = self.remaining_spec_after(reserved);
        if let Some(reason) = exhausted_dimension_of(&remaining, &self.spec) {
            return Err(BudgetStop {
                reason,
                usage: self.usage,
            });
        }
        let effective = intersect_specs(remaining, child_spec);
        {
            let mut pool = self.reservations.lock().expect("lease reservation pool");
            pool.reserved.merge(BudgetUsage {
                turns: effective.max_turns.unwrap_or(0),
                tool_calls: effective.max_tool_calls.unwrap_or(0),
                cost_usd_micros: effective.max_cost_usd_micros.unwrap_or(0),
                wall_time_ms: effective.max_wall_time_ms.unwrap_or(0),
            });
        }
        Ok(BudgetLease::new(effective, Arc::clone(&self.reservations)))
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

    /// What this operation still has left, per dimension, after both consumed
    /// usage and capacity reserved by outstanding child leases. A dimension
    /// with nothing left yields `Some(0)` — never a padded minimum — so a
    /// lease can never be granted capacity the parent does not have.
    fn remaining_spec_after(&self, reserved: BudgetUsage) -> BudgetSpec {
        let subtract_turns = |limit: Option<u32>| {
            limit.map(|limit| {
                limit
                    .saturating_sub(self.usage.turns)
                    .saturating_sub(reserved.turns)
            })
        };
        let subtract_tools = |limit: Option<u32>| {
            limit.map(|limit| {
                limit
                    .saturating_sub(self.usage.tool_calls)
                    .saturating_sub(reserved.tool_calls)
            })
        };
        BudgetSpec {
            max_turns: subtract_turns(self.spec.max_turns),
            max_tool_calls: subtract_tools(self.spec.max_tool_calls),
            max_cost_usd_micros: self.spec.max_cost_usd_micros.map(|limit| {
                limit
                    .saturating_sub(self.usage.cost_usd_micros)
                    .saturating_sub(reserved.cost_usd_micros)
            }),
            max_wall_time_ms: self.spec.max_wall_time_ms.map(|limit| {
                limit
                    .saturating_sub(self.usage.wall_time_ms)
                    .saturating_sub(reserved.wall_time_ms)
            }),
        }
    }

    /// Publishes elapsed wall time into usage and stops when the wall-time
    /// dimension is exhausted (or a stop was already latched). The turn
    /// dimension is never a constraint here: a turn already admitted may
    /// keep running its tools while the wall clock ticks. Call after every
    /// provider exchange so a wall-time stop lands promptly.
    pub(crate) fn sync_wall_time(&mut self) -> Result<(), BudgetStop> {
        self.sync_wall_time_inner();
        self.stop_if_wall_time_exhausted().map_or(Ok(()), Err)
    }

    fn stop_if_wall_time_exhausted(&mut self) -> Option<BudgetStop> {
        let reason = match self.state {
            TerminalState::Stopped(reason) => reason,
            TerminalState::Running => {
                let Some(max_wall_time_ms) = self.spec.max_wall_time_ms else {
                    return None;
                };
                if self.usage.wall_time_ms <= max_wall_time_ms {
                    return None;
                }
                StopReason::WallTimeBudget { max_wall_time_ms }
            }
        };
        self.state = TerminalState::Stopped(reason);
        Some(BudgetStop {
            reason,
            usage: self.usage,
        })
    }

    fn sync_wall_time_inner(&mut self) {
        let elapsed_ms = self.started_at.elapsed().as_millis() as u64;
        if elapsed_ms > self.usage.wall_time_ms {
            self.usage.wall_time_ms = elapsed_ms;
        }
    }

    fn stop_if_exhausted(&mut self) -> Option<BudgetStop> {
        let reason = match self.state {
            TerminalState::Stopped(reason) => reason,
            TerminalState::Running => self.exhausted_dimension(false)?,
        };
        self.state = TerminalState::Stopped(reason);
        Some(BudgetStop {
            reason,
            usage: self.usage,
        })
    }

    /// Stop check for in-turn accounting (tool admission, cost recording):
    /// the turn dimension is never a constraint here because the current turn
    /// was already admitted; a latched stop still short-circuits.
    fn stop_if_exhausted_without_turn_dimension(&mut self) -> Option<BudgetStop> {
        let reason = match self.state {
            TerminalState::Stopped(reason) => reason,
            TerminalState::Running => self.exhausted_dimension(true)?,
        };
        self.state = TerminalState::Stopped(reason);
        Some(BudgetStop {
            reason,
            usage: self.usage,
        })
    }

    fn exhausted_dimension(&self, skip_turn: bool) -> Option<StopReason> {
        if !skip_turn
            && let Some(max_turns) = self.spec.max_turns
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
/// contexts. The reservation is held in the parent's shared pool for the
/// lease's lifetime and returned exactly once — by [`BudgetLease::finish`]
/// when the child settles normally, or by `Drop` when it is abandoned — so
/// concurrent children never double-spend the parent and leaked leases never
/// permanently drain it. Consumed usage reports upward via
/// [`BudgetController::merge_child_usage`].
#[derive(Debug)]
pub struct BudgetLease {
    effective_spec: BudgetSpec,
    usage: BudgetUsage,
    reservations: Option<SharedReservations>,
    settled: bool,
}

impl BudgetLease {
    fn new(effective_spec: BudgetSpec, reservations: SharedReservations) -> Self {
        Self {
            effective_spec,
            usage: BudgetUsage::default(),
            reservations: Some(reservations),
            settled: false,
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

    /// Consumed usage receipt for the parent to merge. Settles the
    /// reservation exactly once: the reserved ceiling returns to the parent's
    /// pool (unused capacity is not reported; only consumed usage is), and
    /// `Drop` afterwards is a no-op.
    pub fn finish(mut self) -> BudgetUsage {
        self.settle_reservation();
        self.usage
    }

    /// Returns the reserved ceiling to the parent's pool. Idempotent: called
    /// by both `finish` and `Drop`, but only the first call settles.
    fn settle_reservation(&mut self) {
        if self.settled {
            return;
        }
        self.settled = true;
        if let Some(reservations) = self.reservations.take() {
            let mut pool = reservations.lock().expect("lease reservation pool");
            pool.reserved = BudgetUsage {
                turns: pool
                    .reserved
                    .turns
                    .saturating_sub(self.effective_spec.max_turns.unwrap_or(0)),
                tool_calls: pool
                    .reserved
                    .tool_calls
                    .saturating_sub(self.effective_spec.max_tool_calls.unwrap_or(0)),
                cost_usd_micros: pool
                    .reserved
                    .cost_usd_micros
                    .saturating_sub(self.effective_spec.max_cost_usd_micros.unwrap_or(0)),
                wall_time_ms: pool
                    .reserved
                    .wall_time_ms
                    .saturating_sub(self.effective_spec.max_wall_time_ms.unwrap_or(0)),
            };
        }
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

impl Drop for BudgetLease {
    /// A lease that is abandoned (never `finish`ed) still returns its
    /// reservation so the parent's pool is never permanently drained.
    fn drop(&mut self) {
        self.settle_reservation();
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

/// The first dimension with nothing left to grant (`Some(0)` after consumed
/// usage and outstanding reservations are deducted). The parent still has
/// the dimension configured, so the reason reports the original limit.
fn exhausted_dimension_of(remaining: &BudgetSpec, spec: &BudgetSpec) -> Option<StopReason> {
    if remaining.max_turns == Some(0) {
        return Some(StopReason::TurnBudget {
            max_turns: spec.max_turns.unwrap_or(0),
        });
    }
    if remaining.max_tool_calls == Some(0) {
        return Some(StopReason::ToolCallBudget {
            max_tool_calls: spec.max_tool_calls.unwrap_or(0),
        });
    }
    if remaining.max_cost_usd_micros == Some(0) {
        return Some(StopReason::CostBudget {
            max_cost_usd_micros: spec.max_cost_usd_micros.unwrap_or(0),
        });
    }
    if remaining.max_wall_time_ms == Some(0) {
        return Some(StopReason::WallTimeBudget {
            max_wall_time_ms: spec.max_wall_time_ms.unwrap_or(0),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> BudgetSpec {
        BudgetSpec::default()
    }

    #[test]
    fn concurrent_child_leases_never_double_spend_remaining_budget() {
        let mut parent = BudgetController::new(BudgetSpec {
            max_turns: Some(3),
            ..spec()
        });
        parent.admit_turn().expect("parent turn 1");

        // Two outstanding leases: the first reserves the remaining two turns,
        // so the second finds nothing left and is refused — it never sees
        // capacity the parent does not have.
        let first = parent
            .child_lease(BudgetSpec {
                max_turns: Some(5),
                ..spec()
            })
            .expect("first lease granted");
        assert_eq!(first.spec().max_turns, Some(2));

        let second = parent.child_lease(BudgetSpec {
            max_turns: Some(5),
            ..spec()
        });
        let stop = second.expect_err("second lease must be refused");
        assert!(matches!(
            stop.reason,
            StopReason::TurnBudget { max_turns: 3 }
        ));
    }

    #[test]
    fn settled_lease_returns_reservation_for_later_children() {
        let mut parent = BudgetController::new(BudgetSpec {
            max_turns: Some(3),
            ..spec()
        });
        parent.admit_turn().expect("parent turn 1");

        let first = parent
            .child_lease(BudgetSpec {
                max_turns: Some(5),
                ..spec()
            })
            .expect("first lease granted");
        assert_eq!(first.spec().max_turns, Some(2));
        // Settle without consuming: the full reservation returns.
        let _consumed = first.finish();

        let second = parent
            .child_lease(BudgetSpec {
                max_turns: Some(5),
                ..spec()
            })
            .expect("second lease granted");
        assert_eq!(second.spec().max_turns, Some(2));
    }

    #[test]
    fn dropped_lease_returns_reservation_exactly_once() {
        let mut parent = BudgetController::new(BudgetSpec {
            max_turns: Some(3),
            ..spec()
        });
        parent.admit_turn().expect("parent turn 1");

        // Abandoned without finish: Drop returns the reservation.
        {
            let abandoned = parent
                .child_lease(BudgetSpec {
                    max_turns: Some(5),
                    ..spec()
                })
                .expect("abandoned lease granted");
            assert_eq!(abandoned.spec().max_turns, Some(2));
        }

        let later = parent
            .child_lease(BudgetSpec {
                max_turns: Some(5),
                ..spec()
            })
            .expect("later lease granted");
        assert_eq!(later.spec().max_turns, Some(2));
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
    fn zero_dimensions_normalize_to_unlimited_instead_of_panicking() {
        // The controller is a last-resort safety net: a zero dimension from a
        // buggy caller must not crash the runtime worker. Config decoding
        // rejects zeros with a clear error before this point.
        let controller = BudgetController::new(BudgetSpec {
            max_turns: Some(0),
            max_tool_calls: Some(0),
            max_cost_usd_micros: Some(0),
            max_wall_time_ms: Some(0),
        });
        assert!(controller.is_unlimited());
        let mut controller = controller;
        controller
            .admit_turn()
            .expect("normalized unlimited admits");
    }
}
