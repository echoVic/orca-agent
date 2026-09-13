//! Shared admission control for child agents.
//!
//! `SubagentConfig::max_parallel` started as the width of one synchronous
//! batch window. That bounds a batch, not the session: nothing stopped several
//! batches, detached async launches, UI-triggered continues, and workflow runs
//! from running children at the same time. This module is the single place
//! that answers "may one more child start right now?" for every launch funnel.
//!
//! The durable [`TaskRegistry`] is the source of truth for the running count,
//! so the answer stays correct when a child runs in its own OS process. The
//! in-flight reservation closes the check-then-create race between concurrent
//! admissions; it is released only once the launch has succeeded or failed.

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::tasks::TaskRegistry;

/// Why a launch could not be admitted immediately.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubagentAdmissionError {
    /// The shared running limit is fully occupied.
    CapacityExceeded { limit: usize, running: usize },
}

impl SubagentAdmissionError {
    /// A model-facing explanation that names the limit and what to do next.
    pub fn message(&self) -> String {
        match self {
            Self::CapacityExceeded { limit, running } => format!(
                "subagent capacity is full ({running}/{limit} running). No child was started. \
                 Do not retry immediately: keep working on a non-overlapping part, wait for a \
                 running child with subagent_status, or stop one with task_stop and relaunch."
            ),
        }
    }
}

/// Shared running-child limit and reservation counter for one task registry.
#[derive(Debug, Default)]
pub struct SubagentAdmission {
    /// Zero until the first admission attempt supplies the configured limit.
    limit: AtomicUsize,
    /// Slots reserved by an admission in progress but not yet visible as a
    /// durable task record.
    in_flight: AtomicUsize,
}

impl SubagentAdmission {
    /// Records the shared ceiling. `SubagentConfig::normalized` already floors
    /// `max_parallel` at one; a caller that bypassed normalization still gets a
    /// usable limit here.
    pub fn set_limit(&self, limit: usize) {
        self.limit.store(limit.max(1), Ordering::SeqCst);
    }

    pub fn limit(&self) -> usize {
        self.limit.load(Ordering::SeqCst).max(1)
    }

    /// Children occupying the limit right now. `registry` supplies the durable
    /// count, which stays correct across worker processes.
    pub fn running(&self, registry: &TaskRegistry) -> usize {
        registry.active_detached_subagent_count() + self.in_flight.load(Ordering::SeqCst)
    }

    /// Reserves one slot for a launch that is about to create its task record.
    ///
    /// The reservation counts against the limit until it is dropped, so a
    /// second launch cannot observe the same free slot before the first one is
    /// registered durably.
    pub fn admit<'a>(
        &'a self,
        registry: &TaskRegistry,
        limit: usize,
    ) -> Result<SubagentAdmissionReservation<'a>, SubagentAdmissionError> {
        self.set_limit(limit);
        let running = self.running(registry);
        if running >= self.limit() {
            return Err(SubagentAdmissionError::CapacityExceeded {
                limit: self.limit(),
                running,
            });
        }
        self.in_flight.fetch_add(1, Ordering::SeqCst);
        Ok(SubagentAdmissionReservation { admission: self })
    }
}

/// One reserved child slot. Dropping it releases the reservation.
pub struct SubagentAdmissionReservation<'a> {
    admission: &'a SubagentAdmission,
}

impl std::fmt::Debug for SubagentAdmissionReservation<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SubagentAdmissionReservation")
            .finish_non_exhaustive()
    }
}

impl Drop for SubagentAdmissionReservation<'_> {
    fn drop(&mut self) {
        self.admission.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limit_never_reaches_zero() {
        let registry = TaskRegistry::new("admission-limit".to_string());
        let admission = SubagentAdmission::default();
        assert_eq!(admission.limit(), 1);
        let _ = admission.admit(&registry, 3);
        assert_eq!(admission.limit(), 3);
        let _ = admission.admit(&registry, 0);
        assert_eq!(admission.limit(), 1);
    }

    #[test]
    fn reservations_are_released_when_dropped() {
        let registry = TaskRegistry::new("admission-free".to_string());
        let admission = SubagentAdmission::default();

        assert_eq!(admission.running(&registry), 0);
        let first = admission.admit(&registry, 2).expect("first admission");
        assert_eq!(admission.running(&registry), 1);
        drop(first);
        assert_eq!(admission.running(&registry), 0);
        admission.admit(&registry, 2).expect("slot is reusable");
    }

    #[test]
    fn in_flight_reservations_cannot_oversubscribe_the_limit() {
        let registry = TaskRegistry::new("admission-race".to_string());
        let admission = SubagentAdmission::default();

        let held = admission.admit(&registry, 1).expect("first reservation");
        let second = admission.admit(&registry, 1).expect_err("second refused");
        assert_eq!(
            second,
            SubagentAdmissionError::CapacityExceeded {
                limit: 1,
                running: 1
            }
        );
        drop(held);
        admission.admit(&registry, 1).expect("slot is free again");
    }

    #[test]
    fn capacity_error_names_the_limit_forbids_retry_and_says_nothing_started() {
        let message = SubagentAdmissionError::CapacityExceeded {
            limit: 2,
            running: 2,
        }
        .message();

        assert!(message.contains("2/2"));
        assert!(message.contains("No child was started"));
        assert!(message.contains("Do not retry immediately"));
        assert!(message.contains("subagent_status"));
    }
}
