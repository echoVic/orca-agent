use std::sync::{Arc, Mutex};

use orca_core::cost_types::UsageTotals;
use orca_core::event_schema::EventFactory;

use super::RuntimeThread;

pub(super) struct ThreadActorState {
    pub(super) thread: RuntimeThread,
    pub(super) events: EventFactory,
}

impl ThreadActorState {
    pub(super) fn new(thread: RuntimeThread) -> (Self, RuntimeUsageLedger) {
        let usage_ledger = RuntimeUsageLedger::new(thread.session().aggregate_usage_totals());
        let events = thread.event_factory();
        (Self { thread, events }, usage_ledger)
    }
}

pub(super) fn retain_recovered_background_approvals(
    controller: &mut super::ResidentBackgroundController,
    resident_surface: Option<&super::ResidentSurfaceState>,
) {
    for (operation_id, pending) in resident_surface
        .map(|resident| {
            super::recovered_background_approval_resolutions(
                resident.coordinator.state().snapshot(),
            )
        })
        .unwrap_or_default()
    {
        controller.retain_approval_resolution(operation_id, pending);
    }
}

#[derive(Clone, Debug)]
pub(super) struct RuntimeUsageLedger {
    totals: Arc<Mutex<UsageTotals>>,
}

impl RuntimeUsageLedger {
    pub(super) fn new(totals: UsageTotals) -> Self {
        Self {
            totals: Arc::new(Mutex::new(totals)),
        }
    }

    pub(super) fn add(&self, usage: UsageTotals) -> UsageTotals {
        let mut totals = self
            .totals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        totals.input_tokens = totals.input_tokens.saturating_add(usage.input_tokens);
        totals.output_tokens = totals.output_tokens.saturating_add(usage.output_tokens);
        totals.cache_tokens = totals.cache_tokens.saturating_add(usage.cache_tokens);
        totals.estimated_cost_usd += usage.estimated_cost_usd;
        *totals
    }

    pub(super) fn totals(&self) -> UsageTotals {
        *self
            .totals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::RuntimeUsageLedger;
    use orca_core::cost_types::UsageTotals;

    #[test]
    fn usage_ledger_accumulates_with_saturation() {
        let ledger = RuntimeUsageLedger::new(UsageTotals {
            input_tokens: u64::MAX,
            output_tokens: 2,
            cache_tokens: 3,
            estimated_cost_usd: 0.25,
        });

        let totals = ledger.add(UsageTotals {
            input_tokens: 1,
            output_tokens: 4,
            cache_tokens: 5,
            estimated_cost_usd: 0.5,
        });

        assert_eq!(totals.input_tokens, u64::MAX);
        assert_eq!(totals.output_tokens, 6);
        assert_eq!(totals.cache_tokens, 8);
        assert!((totals.estimated_cost_usd - 0.75).abs() < f64::EPSILON);
        assert_eq!(ledger.totals(), totals);
    }
}
