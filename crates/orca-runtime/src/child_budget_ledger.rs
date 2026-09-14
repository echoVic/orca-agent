//! Durable reservations for children that outlive their submission call.
//! A missing receipt retains its reservation; process death never refunds it.

use orca_core::budget::{BudgetSpec, BudgetUsage};
use orca_platform::fs::{AtomicWritePolicy, ExclusiveFileLock, atomic_write};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, io, path::PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChildBudgetReservation {
    pub(crate) path: PathBuf,
    pub(crate) id: String,
    pub(crate) spec: BudgetSpec,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Entry {
    pub spec: BudgetSpec,
    pub receipt: Option<BudgetUsage>,
    pub accounted: bool,
    #[serde(default)]
    pub task_id: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct Ledger {
    entries: BTreeMap<String, Entry>,
}

fn transaction<T>(
    path: &PathBuf,
    apply: impl FnOnce(&mut Ledger) -> io::Result<T>,
) -> io::Result<T> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let _lock =
        ExclusiveFileLock::acquire(&path.with_extension("lock")).map_err(io::Error::other)?;
    let mut ledger = if path.exists() {
        serde_json::from_slice(&fs::read(path)?).map_err(io::Error::other)?
    } else {
        Ledger::default()
    };
    let result = apply(&mut ledger)?;
    atomic_write(
        path,
        &serde_json::to_vec(&ledger).map_err(io::Error::other)?,
        AtomicWritePolicy::NoFollow,
    )
    .map_err(io::Error::other)?;
    Ok(result)
}

pub(crate) fn reserved(spec: BudgetSpec) -> BudgetUsage {
    BudgetUsage {
        turns: spec.max_turns.unwrap_or(0),
        tool_calls: spec.max_tool_calls.unwrap_or(0),
        cost_usd_micros: spec.max_cost_usd_micros.unwrap_or(0),
        wall_time_ms: 0,
    }
}

pub(crate) fn entries(path: &PathBuf) -> io::Result<BTreeMap<String, Entry>> {
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let _lock =
        ExclusiveFileLock::acquire(&path.with_extension("lock")).map_err(io::Error::other)?;
    let ledger: Ledger = serde_json::from_slice(&fs::read(path)?).map_err(io::Error::other)?;
    Ok(ledger.entries)
}

pub(crate) fn acknowledge(path: &PathBuf, id: &str) -> io::Result<()> {
    transaction(path, |ledger| {
        let entry = ledger
            .entries
            .get_mut(id)
            .ok_or_else(|| io::Error::other("unknown child budget reservation"))?;
        if entry.receipt.is_none() {
            return Err(io::Error::other(
                "cannot account for a missing child receipt",
            ));
        }
        entry.accounted = true;
        Ok(())
    })
}

impl ChildBudgetReservation {
    pub(crate) fn existing(path: PathBuf, id: String) -> io::Result<Option<Self>> {
        Ok(entries(&path)?.get(&id).map(|entry| Self {
            path,
            id,
            spec: entry.spec,
        }))
    }
    pub(crate) fn bound_task(&self) -> io::Result<Option<String>> {
        entries(&self.path)?
            .get(&self.id)
            .map(|entry| entry.task_id.clone())
            .ok_or_else(|| io::Error::other("missing child budget binding"))
    }
    pub(crate) fn settled_without_submission(&self) -> io::Result<bool> {
        entries(&self.path)?
            .get(&self.id)
            .map(|entry| entry.task_id.is_none() && entry.receipt.is_some())
            .ok_or_else(|| io::Error::other("missing child budget binding"))
    }
    pub(crate) fn observation(&self) -> serde_json::Value {
        match entries(&self.path).and_then(|mut entries| {
            entries
                .remove(&self.id)
                .ok_or_else(|| io::Error::other("missing child budget reservation"))
        }) {
            Ok(entry) => serde_json::json!({"limit": entry.spec, "actual_usage": entry.receipt,
                "settlement": if entry.receipt.is_some() { "settled" } else { "unsettled" },
                "parent_accounted": entry.accounted}),
            Err(_) => {
                serde_json::json!({"limit": self.spec, "actual_usage": null, "settlement": "unavailable"})
            }
        }
    }
    pub(crate) fn reserve(
        path: PathBuf,
        id: String,
        spec: BudgetSpec,
        limit: BudgetSpec,
        consumed: BudgetUsage,
    ) -> io::Result<Self> {
        transaction(&path, |ledger| {
            if let Some(existing) = ledger.entries.get(&id) {
                if existing.spec != spec {
                    return Err(io::Error::other("child reservation changed during replay"));
                }
                return Ok(());
            }
            let mut charged = consumed;
            for entry in ledger.entries.values().filter(|entry| !entry.accounted) {
                charged.merge(entry.receipt.unwrap_or_else(|| reserved(entry.spec)));
            }
            charged.merge(reserved(spec));
            if limit.max_turns.is_some_and(|max| charged.turns > max)
                || limit
                    .max_tool_calls
                    .is_some_and(|max| charged.tool_calls > max)
                || limit
                    .max_cost_usd_micros
                    .is_some_and(|max| charged.cost_usd_micros > max)
            {
                return Err(io::Error::other(
                    "child budget reservation exceeds unreserved parent capacity",
                ));
            }
            ledger.entries.insert(
                id.clone(),
                Entry {
                    spec,
                    receipt: None,
                    accounted: false,
                    task_id: None,
                },
            );
            Ok(())
        })?;
        Ok(Self { path, id, spec })
    }

    pub(crate) fn bind_task(&self, task_id: &str) -> io::Result<()> {
        transaction(&self.path, |ledger| {
            let entry = ledger
                .entries
                .get_mut(&self.id)
                .ok_or_else(|| io::Error::other("unknown child budget reservation"))?;
            if entry.task_id.as_deref().is_some_and(|id| id != task_id) {
                return Err(io::Error::other("child budget is already bound"));
            }
            entry.task_id = Some(task_id.to_owned());
            Ok(())
        })
    }
    pub(crate) fn refund_unsubmitted(&self) -> io::Result<()> {
        transaction(&self.path, |ledger| {
            let entry = ledger
                .entries
                .get_mut(&self.id)
                .ok_or_else(|| io::Error::other("unknown child budget reservation"))?;
            if entry.task_id.is_none() && entry.receipt.is_none() {
                entry.receipt = Some(BudgetUsage::default());
            }
            Ok(())
        })
    }

    /// Cumulative final receipt, including failed/cancelled work. Identical
    /// retries are harmless; a conflicting bill is never silently accepted.
    pub(crate) fn settle(&self, mut usage: BudgetUsage) -> io::Result<()> {
        usage.wall_time_ms = 0;
        transaction(&self.path, |ledger| {
            let entry = ledger
                .entries
                .get_mut(&self.id)
                .ok_or_else(|| io::Error::other("unknown child budget reservation"))?;
            if entry.spec != self.spec {
                return Err(io::Error::other("child budget binding mismatch"));
            }
            if let Some(previous) = entry.receipt {
                if previous != usage {
                    return Err(io::Error::other("conflicting child budget receipt"));
                }
            } else {
                entry.receipt = Some(usage);
            }
            Ok(())
        })
    }
}

/// A queued launch has not executed. Dropping its closure may safely return
/// the allocation; once started, only an explicit receipt can settle it.
pub(crate) struct PendingChildBudget {
    pub reservation: Option<ChildBudgetReservation>,
    started: bool,
}
impl PendingChildBudget {
    pub fn new(reservation: Option<ChildBudgetReservation>) -> Self {
        Self {
            reservation,
            started: false,
        }
    }
    pub fn start(&mut self) {
        self.started = true;
    }
}
impl Drop for PendingChildBudget {
    fn drop(&mut self) {
        if !self.started
            && let Some(reservation) = &self.reservation
        {
            let _ = reservation.settle(BudgetUsage::default());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parallel_reservations_cannot_double_spend_and_receipts_survive_reopen() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("children.json");
        let limit = BudgetSpec {
            max_cost_usd_micros: Some(32_000),
            ..Default::default()
        };
        let spec = BudgetSpec {
            max_cost_usd_micros: Some(1_000),
            ..Default::default()
        };
        let workers: Vec<_> = (0..40)
            .map(|id| {
                let path = path.clone();
                std::thread::spawn(move || {
                    ChildBudgetReservation::reserve(
                        path,
                        id.to_string(),
                        spec,
                        limit,
                        BudgetUsage::default(),
                    )
                })
            })
            .collect();
        let leases: Vec<_> = workers
            .into_iter()
            .filter_map(|worker| worker.join().unwrap().ok())
            .collect();
        assert_eq!(leases.len(), 32);
        let receipt = BudgetUsage {
            cost_usd_micros: 250,
            ..Default::default()
        };
        leases[0].settle(receipt).unwrap();
        leases[0].settle(receipt).unwrap();
        assert!(leases[0].settle(BudgetUsage::default()).is_err());
        let state = entries(&path).unwrap();
        assert_eq!(state[&leases[0].id].receipt, Some(receipt));
        assert_eq!(
            state
                .values()
                .filter(|entry| entry.receipt.is_none())
                .count(),
            31
        );
    }
}
