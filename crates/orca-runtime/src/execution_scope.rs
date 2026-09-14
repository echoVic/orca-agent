//! Scope-limited execution capacity with queueing instead of refusal.
//!
//! One root task tree has one execution scope. Every child in that tree — a
//! direct child, a grandchild, a workflow's LLM subtask, a UI-triggered
//! continue, a resumed attempt — competes for the same set of execution
//! leases. Synchronous and background launches differ only in whether the
//! caller waits; they do not each own a pool.
//!
//! Capacity pressure queues. A submitted task is accepted, recorded as
//! `queued`, and started when a lease frees; the caller is never asked to
//! resubmit and never sees a capacity refusal for a legal task.
//!
//! ## Why the limits are separate
//!
//! - `max_running` bounds execution leases. A lease is held while a task can
//!   start, call a model, or run a tool, and stays held through `stopping`
//!   until execution is actually silent.
//! - `max_queued` bounds accepted work that has not started, so a burst cannot
//!   accumulate unbounded pending work.
//! - `max_live_tasks` bounds every non-terminal child. A child that released
//!   its lease while waiting for its own children is neither running nor
//!   queued, so without this limit a tree could free execution capacity and
//!   still accumulate unbounded processes and conversations.

use std::cell::Cell;
use std::sync::{Condvar, Mutex, MutexGuard};
use std::time::Duration;

use orca_core::cancel::CancelToken;
use orca_core::subagent_config::SubagentLimits;
use orca_core::task_types::{TaskStatus, TaskType};

use crate::task_view::{ExecutionState, WaitReason};
use crate::tasks::{TaskRecord, TaskRegistry};

/// Serialises admission decisions for one execution scope and wakes the
/// callers that are waiting for capacity.
///
/// Counting from a shared atomic is not an admission decision: two threads can
/// both read `running = 31`, both decide they fit, and both start. Every
/// decision that reserves a lease therefore runs while this lock is held, and
/// the count it reads is the same one the decision is recorded against.
///
/// The same lock carries the fairness cursor. Rotation across parent branches
/// is state, not a property of one snapshot, so a branch that was just served
/// starts at the back of the next round even when the scope frees one lease at
/// a time.
///
/// One arbiter belongs to one root task tree, which is why it lives on the
/// [`TaskRegistry`] rather than in a global.
pub struct ScopeArbiter {
    state: Mutex<ArbiterState>,
    signal: Condvar,
    dispatcher: Mutex<Option<std::sync::mpsc::Sender<PendingLaunch>>>,
    scheduled: std::sync::Arc<Mutex<std::collections::HashSet<String>>>,
}

#[derive(Default)]
struct ArbiterState {
    /// Identity survives changes in the pending branch list.
    last_branch: Option<String>,
    /// Bumped on every capacity-relevant change so waiters can tell a real
    /// wakeup from a spurious one.
    generation: u64,
}

impl Default for ScopeArbiter {
    fn default() -> Self {
        Self::new()
    }
}

struct PendingLaunch {
    registry: TaskRegistry,
    task_id: String,
    limits: SubagentLimits,
    launch: Box<dyn FnOnce() + Send>,
}

impl ScopeArbiter {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(ArbiterState::default()),
            signal: Condvar::new(),
            dispatcher: Mutex::new(None),
            scheduled: std::sync::Arc::new(Mutex::new(std::collections::HashSet::new())),
        }
    }

    /// One dispatcher per scope, not one thread/process for every queued job.
    pub(crate) fn enqueue(
        &self,
        registry: TaskRegistry,
        task_id: String,
        limits: SubagentLimits,
        launch: impl FnOnce() + Send + 'static,
    ) -> std::io::Result<()> {
        {
            let mut scheduled = self.scheduled.lock().unwrap_or_else(|p| p.into_inner());
            if !scheduled.insert(task_id.clone()) {
                return Ok(());
            }
        }
        let mut sender = self.dispatcher.lock().unwrap_or_else(|p| p.into_inner());
        if sender.is_none() {
            let (tx, rx) = std::sync::mpsc::channel::<PendingLaunch>();
            let scheduled = std::sync::Arc::clone(&self.scheduled);
            std::thread::Builder::new()
                .name("orca-child-dispatch".into())
                .spawn(move || {
                    let mut pending = Vec::<PendingLaunch>::new();
                    let mut active = Vec::<(TaskRegistry, String)>::new();
                    loop {
                        match rx.recv_timeout(Duration::from_millis(25)) {
                            Ok(job) => pending.push(job),
                            Err(std::sync::mpsc::RecvTimeoutError::Disconnected)
                                if pending.is_empty() && active.is_empty() =>
                            {
                                break;
                            }
                            Err(_) => {}
                        }
                        pending.extend(rx.try_iter());
                        active.retain(|(registry, id)| {
                            let Some(record) = registry.get(id) else {
                                return false;
                            };
                            if !occupies_scope(record.status) {
                                return false;
                            }
                            if record.status != TaskStatus::Stopping
                                && registry.deadline_expired(id)
                            {
                                let _ = registry.request_stop_tree(id);
                            }
                            true
                        });
                        let mut index = 0;
                        while index < pending.len() {
                            let job = &pending[index];
                            let scope = job.registry.execution_scope(&job.limits);
                            scope.stop_expired(chrono::Utc::now().timestamp_millis());
                            let record = job.registry.get(&job.task_id);
                            if record.as_ref().is_none_or(|record| {
                                !occupies_scope(record.status)
                                    || record.control.cancel.is_cancelled()
                            }) {
                                let job = pending.remove(index);
                                scheduled
                                    .lock()
                                    .unwrap_or_else(|p| p.into_inner())
                                    .remove(&job.task_id);
                                let _ = job
                                    .registry
                                    .stop(&job.task_id, "cancelled before dispatch".into());
                                continue;
                            }
                            if scope.acquire(&job.task_id, chrono::Utc::now().timestamp_millis())
                                == Admission::Queued
                            {
                                index += 1;
                                continue;
                            }
                            let job = pending.remove(index);
                            let registry = job.registry.clone();
                            let task_id = job.task_id.clone();
                            active.push((registry.clone(), task_id.clone()));
                            let panic_registry = registry.clone();
                            let panic_task = task_id.clone();
                            let completed_scheduled = std::sync::Arc::clone(&scheduled);
                            if let Err(error) = std::thread::Builder::new()
                                .name(format!("orca-child-{task_id}"))
                                .spawn(move || {
                                    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                                        job.launch,
                                    ))
                                    .is_err()
                                    {
                                        let _ = panic_registry.mark_execution_indeterminate(
                                            &panic_task,
                                            "child launch panicked; inspect task before retrying"
                                                .into(),
                                        );
                                    }
                                    completed_scheduled
                                        .lock()
                                        .unwrap_or_else(|p| p.into_inner())
                                        .remove(&panic_task);
                                })
                            {
                                scheduled
                                    .lock()
                                    .unwrap_or_else(|p| p.into_inner())
                                    .remove(&task_id);
                                let _ = registry
                                    .fail(&task_id, format!("cannot start child worker: {error}"));
                            }
                        }
                    }
                })?;
            *sender = Some(tx);
        }
        let result = sender
            .as_ref()
            .unwrap()
            .send(PendingLaunch {
                registry,
                task_id: task_id.clone(),
                limits,
                launch: Box::new(launch),
            })
            .map_err(|_| std::io::Error::other("child dispatcher stopped"));
        if result.is_err() {
            self.scheduled
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&task_id);
        }
        result
    }

    fn lock(&self) -> MutexGuard<'_, ArbiterState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Runs an admission decision with this scope's lock held.
    ///
    /// A decision both reads the counts and writes the record they describe,
    /// so a mutation made inside the decision must not take the lock again:
    /// it leaves a wakeup request behind and this epilogue delivers it once
    /// the lock is released. Decisions do not nest.
    fn decide<T>(&self, decide: impl FnOnce(&mut ArbiterState) -> T) -> T {
        let mut state = self.lock();
        let was_deciding = IN_DECISION.with(|flag| flag.replace(true));
        DECISION_DIRTY.with(|dirty| dirty.set(false));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| decide(&mut state)));
        let dirty = DECISION_DIRTY.with(|dirty| dirty.replace(false));
        IN_DECISION.with(|flag| flag.set(was_deciding));
        if dirty {
            state.generation = state.generation.wrapping_add(1);
        }
        drop(state);
        if dirty {
            self.signal.notify_all();
        }
        match result {
            Ok(result) => result,
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }

    pub(crate) fn serialize<T>(&self, change: impl FnOnce() -> T) -> T {
        self.decide(|_| change())
    }

    /// Announces that a fact admission depends on changed: a task was
    /// submitted, started, stopped, or finished, or a lease was released.
    ///
    /// Called after the registry mutation has been committed and its lock
    /// released, so the arbiter lock is always taken last.
    pub fn announce(&self) {
        if IN_DECISION.with(Cell::get) {
            // This thread already holds the scope lock; the decision that
            // reads these facts delivers the wakeup when it finishes.
            DECISION_DIRTY.with(|dirty| dirty.set(true));
            return;
        }
        {
            let mut state = self.lock();
            state.generation = state.generation.wrapping_add(1);
        }
        self.signal.notify_all();
    }

    /// Waits for the next announcement.
    ///
    /// Returns `true` when something changed and the caller should look again,
    /// `false` when the wait ended without news (timeout) or the caller was
    /// cancelled. The timeout only bounds how long cancellation and a missed
    /// announcement can delay progress; it is not an execution deadline and
    /// never cancels the work itself.
    pub fn wait(&self, cancel: Option<&CancelToken>, timeout: Duration) -> bool {
        if cancel.is_some_and(CancelToken::is_cancelled) {
            return false;
        }
        let state = self.lock();
        let observed = state.generation;
        let (state, _) = self
            .signal
            .wait_timeout_while(state, timeout, |state| state.generation == observed)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.generation != observed
    }
}

thread_local! {
    /// Whether this thread is inside [`ScopeArbiter::decide`].
    static IN_DECISION: Cell<bool> = const { Cell::new(false) };
    /// Whether a mutation inside the current decision asked for a wakeup.
    static DECISION_DIRTY: Cell<bool> = const { Cell::new(false) };
}

/// What happened to a submission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Admission {
    /// The task holds an execution lease and may start.
    Started,
    /// The task is accepted and waiting for a lease.
    Queued,
}

/// An accepted submission: the record exists and the lease question is
/// already answered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Submission {
    pub task_id: String,
    pub admission: Admission,
    /// The tree was already cancelled, so the record exists but must not run.
    pub cancelled: bool,
}

/// A submission the scope refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdmissionRefusal {
    Unavailable,
    /// The scope already holds its maximum queued work.
    QueueFull {
        queued: usize,
        limit: usize,
    },
    /// The scope already holds its maximum non-terminal children.
    LiveTaskLimit {
        live: usize,
        limit: usize,
    },
}

impl AdmissionRefusal {
    /// A model-facing explanation that says which limit was hit and what still
    /// works. A refusal is terminal for this submission, so the caller must
    /// not retry blindly.
    pub fn message(self) -> String {
        match self {
            Self::Unavailable => {
                "execution scope storage is unavailable; no task was accepted".into()
            }
            Self::QueueFull { queued, limit } => format!(
                "the task queue for this task tree is full ({queued}/{limit} waiting to start). \
                 The task was not accepted. Existing tasks can still be observed with task_list, \
                 waited on with task_wait, and stopped with task_stop; submit again once one of \
                 them finishes."
            ),
            Self::LiveTaskLimit { live, limit } => format!(
                "this task tree already holds its maximum non-terminal child tasks ({live}/{limit}). \
                 The task was not accepted. Existing tasks can still be observed with task_list, \
                 waited on with task_wait, and stopped with task_stop."
            ),
        }
    }
}

/// A scope's live capacity view.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScopeCapacity {
    /// False means counts are conservative reservations, not observed usage.
    pub available: bool,
    pub running: usize,
    pub queued: usize,
    pub live: usize,
    pub limits: SubagentLimits,
}

impl ScopeCapacity {
    pub fn to_json(self) -> serde_json::Value {
        serde_json::json!({
            "available": self.available,
            "running": self.running,
            "limit": self.limits.max_running,
            "queued": self.queued,
            "queued_limit": self.limits.max_queued,
            "live": self.live,
            "live_limit": self.limits.max_live_tasks,
        })
    }
}

/// Counts and ordering for one root task tree.
///
/// The counts are derived from the durable task registry rather than a
/// separate counter, so they survive a restart and cannot drift from the
/// facts the model is shown. The queue order is a projection of the same
/// records: `TaskRecord.created_at_ms` decides first-come order, and the
/// parent branch decides rotation.
pub struct ExecutionScope<'a> {
    registry: &'a TaskRegistry,
    arbiter: &'a ScopeArbiter,
    limits: SubagentLimits,
}

impl<'a> ExecutionScope<'a> {
    pub fn new(registry: &'a TaskRegistry, limits: SubagentLimits) -> Self {
        Self {
            registry,
            arbiter: registry.scope_arbiter(),
            limits: limits.normalized(),
        }
    }

    /// This scope's admission arbiter, for callers that must announce a
    /// capacity change made outside the registry.
    pub fn arbiter(&self) -> &'a ScopeArbiter {
        self.arbiter
    }

    /// Every child task in this scope, including ones that released a lease
    /// while waiting for their own children.
    fn children(&self) -> Vec<TaskRecord> {
        self.snapshot()
            .map(|snapshot| snapshot.0)
            .unwrap_or_default()
    }

    /// One consistent view of the tree: the child records plus the set of
    /// parents that have live children.
    ///
    /// A scope query reads the registry once, so a large tree does not cost a
    /// lookup per task and every count in one query agrees with the others.
    fn snapshot(&self) -> Option<(Vec<TaskRecord>, std::collections::HashSet<String>)> {
        let summaries = self.registry.records_snapshot().ok()?;
        let mut waiting_parents = std::collections::HashSet::new();
        for summary in &summaries {
            let live = !matches!(
                summary.status,
                TaskStatus::Stopped
                    | TaskStatus::Completed
                    | TaskStatus::Failed
                    | TaskStatus::Cancelled
            );
            if live && let Some(parent) = summary.parent_task_id.as_ref() {
                waiting_parents.insert(parent.clone());
            }
        }
        let children = summaries
            .iter()
            .filter(|summary| summary.task_type == TaskType::Subagent)
            .cloned()
            .collect();
        Some((children, waiting_parents))
    }

    /// Whether a parent has live children, from one snapshot.
    fn waits_on_children(
        waiting_parents: &std::collections::HashSet<String>,
        task_id: &str,
    ) -> bool {
        waiting_parents.contains(task_id)
    }

    fn execution_state(
        _waiting_parents: &std::collections::HashSet<String>,
        record: &TaskRecord,
    ) -> ExecutionState {
        if record.continuation_indeterminate {
            return ExecutionState::Indeterminate;
        }
        match record.status {
            TaskStatus::Queued => ExecutionState::Queued,
            TaskStatus::Running => ExecutionState::Running,
            TaskStatus::Paused => ExecutionState::Waiting,
            TaskStatus::Stopping => ExecutionState::Stopping,
            TaskStatus::Stopped
            | TaskStatus::Completed
            | TaskStatus::Failed
            | TaskStatus::Cancelled => ExecutionState::Terminal,
            TaskStatus::ApprovalRequired => ExecutionState::Waiting,
        }
    }

    /// Tasks holding an execution lease right now.
    ///
    /// A parent blocked on its own children has released its lease, so it does
    /// not count here; that is what lets its descendants be admitted in the
    /// capacity it returned. `stopping` still counts, because a cancellation
    /// request is not proof that execution stopped.
    pub fn running(&self) -> usize {
        let Some((children, waiting_parents)) = self.snapshot() else {
            return self.limits.max_running;
        };
        children
            .iter()
            .filter(|record| {
                Self::execution_state(&waiting_parents, record).holds_execution_lease()
            })
            .count()
    }

    /// The tree-aware state of one child, from a snapshot.
    fn view(&self, record: &TaskRecord) -> ExecutionState {
        // Lease ownership is explicit in this record. Merely having live
        // descendants never releases it and needs no additional disk scan.
        Self::execution_state(&std::collections::HashSet::new(), record)
    }

    /// Accepted work waiting for a first lease.
    pub fn queued(&self) -> Vec<TaskRecord> {
        let mut queued = self
            .children()
            .into_iter()
            .filter(|record| self.view(record) == ExecutionState::Queued)
            .collect::<Vec<_>>();
        // First come, first served within a branch; branches rotate so one
        // parent that keeps submitting cannot monopolise the scope. Order uses
        // the submission sequence, not the clock: several tasks submitted in
        // one turn share a millisecond.
        queued.sort_by_key(|record| record.sequence);
        queued
    }

    /// Every non-terminal child in this scope.
    pub fn live(&self) -> usize {
        self.children()
            .iter()
            .filter(|record| self.view(record) != ExecutionState::Terminal)
            .count()
    }

    pub fn capacity(&self) -> ScopeCapacity {
        let Some((children, _)) = self.snapshot() else {
            return ScopeCapacity {
                available: false,
                running: self.limits.max_running,
                queued: self.limits.max_queued,
                live: self.limits.max_live_tasks,
                limits: self.limits,
            };
        };
        ScopeCapacity {
            available: true,
            running: children
                .iter()
                .filter(|record| self.view(record).holds_execution_lease())
                .count(),
            queued: children
                .iter()
                .filter(|record| {
                    self.view(record) == ExecutionState::Queued && record.started_at_ms.is_none()
                })
                .count(),
            live: children
                .iter()
                .filter(|record| self.view(record) != ExecutionState::Terminal)
                .count(),
            limits: self.limits,
        }
    }

    /// Test-only view of this scope's child records with their states.
    #[cfg(test)]
    fn running_for_test(&self) -> Vec<String> {
        let (children, waiting_parents) = self.snapshot().expect("test scope snapshot");
        children
            .into_iter()
            .filter(|record| {
                Self::execution_state(&waiting_parents, record).holds_execution_lease()
            })
            .map(|record| record.id)
            .collect()
    }

    /// Stops every accepted task whose deadline has passed.
    ///
    /// A deadline that expires while a task is still queued must not start it
    /// and must not leave it waiting forever: the work is recorded as stopped
    /// with the deadline as the reason. A task that already holds a lease is
    /// left to the stop path, which confirms execution is silent before the
    /// lease is released.
    pub fn stop_expired(&self, now_ms: i64) -> Vec<String> {
        let expired = self
            .children()
            .into_iter()
            .filter(|record| occupies_scope(record.status) && record.status != TaskStatus::Stopping)
            .filter(|record| deadline_expired(record, now_ms))
            .collect::<Vec<_>>();
        for record in &expired {
            if record.started_at_ms.is_none() {
                let _ = self.registry.stop(
                    &record.id,
                    "execution deadline passed before the task started".into(),
                );
            } else {
                let _ = self.registry.request_stop_tree(&record.id);
            }
        }
        expired.into_iter().map(|record| record.id).collect()
    }

    /// Whether another child may be accepted at all.
    pub fn refuses(&self) -> Option<AdmissionRefusal> {
        let Some((children, _)) = self.snapshot() else {
            return Some(AdmissionRefusal::Unavailable);
        };
        let queued = children
            .iter()
            .filter(|record| record.status == TaskStatus::Queued && record.started_at_ms.is_none())
            .count();
        if queued >= self.limits.max_queued {
            return Some(AdmissionRefusal::QueueFull {
                queued,
                limit: self.limits.max_queued,
            });
        }
        let live = children
            .iter()
            .filter(|record| self.view(record) != ExecutionState::Terminal)
            .count();
        if live >= self.limits.max_live_tasks {
            return Some(AdmissionRefusal::LiveTaskLimit {
                live,
                limit: self.limits.max_live_tasks,
            });
        }
        None
    }

    /// Decides how a freshly recorded task proceeds.
    ///
    /// The task record already exists, so this never "fails to start" work
    /// that was accepted: it either holds a lease now or waits for one.
    pub fn classify(&self, task_id: &str) -> Option<Admission> {
        let record = self.registry.get(task_id)?;
        if !occupies_scope(record.status) || record.continuation_indeterminate {
            return None;
        }
        if self.view(&record) != ExecutionState::Queued {
            return Some(Admission::Started);
        }
        // `running` already excludes this task while it is queued.
        if self.running() < self.limits.max_running {
            Some(Admission::Started)
        } else {
            Some(Admission::Queued)
        }
    }

    /// Queued tasks that may start now, in fair order.
    ///
    /// Called after a lease is released. It returns at most the number of
    /// leases that actually freed, so releasing one slot admits one task
    /// rather than draining the whole queue.
    ///
    /// A task whose deadline already passed is not a candidate: starting it
    /// would launch work that is out of time before it begins. It is stopped
    /// by [`ExecutionScope::stop_expired`] instead.
    pub fn ready_to_start(&self, now_ms: i64) -> Vec<String> {
        let free = self.limits.max_running.saturating_sub(self.running());
        if free == 0 {
            return Vec::new();
        }
        let Ok(_lock) = self.registry.execution_scope_lock() else {
            return Vec::new();
        };
        self.arbiter.decide(|state| {
            let Ok(durable) = self.registry.execution_scope_cursor() else {
                return Vec::new();
            };
            self.fair_order(
                durable.as_deref().or(state.last_branch.as_deref()),
                free,
                now_ms,
            )
            .0
        })
    }

    /// The next `free` queued tasks in fair order, and the branch list the
    /// rotation was computed over.
    ///
    /// Branches are the parent task ids in submission order, so one parent
    /// that keeps submitting cannot hold the front of the queue: each round
    /// takes one task per branch, starting the round at `cursor`.
    fn fair_order(
        &self,
        last_branch: Option<&str>,
        free: usize,
        now_ms: i64,
    ) -> (Vec<String>, Vec<String>) {
        // One snapshot for the whole decision: this runs on every wakeup, and
        // a per-task query would make a 512-task tree quadratic.
        let Some((children, waiting_parents)) = self.snapshot() else {
            return (Vec::new(), Vec::new());
        };
        let mut queued = children
            .into_iter()
            .filter(|record| {
                Self::execution_state(&waiting_parents, record) == ExecutionState::Queued
            })
            // A restricted parent still waiting for its own children is not
            // runnable: it is not competing for a lease until they settle.
            .filter(|record| {
                record.wait_reason != Some(WaitReason::Dependency)
                    || (record.pending_wait.is_none()
                        && !Self::waits_on_children(&waiting_parents, &record.id))
            })
            .filter(|record| !deadline_expired(record, now_ms))
            .collect::<Vec<_>>();
        queued.sort_by_key(|record| record.sequence);
        let mut branches: Vec<String> = Vec::new();
        for record in &queued {
            let branch = branch_of(record);
            if !branches.contains(&branch) {
                branches.push(branch);
            }
        }
        if branches.is_empty() {
            return (Vec::new(), branches);
        }
        let rotation = last_branch
            .and_then(|last| branches.iter().position(|branch| branch == last))
            .map(|index| (index + 1) % branches.len())
            .unwrap_or(0);
        let mut remaining = queued;
        let mut admitted = Vec::new();
        while admitted.len() < free {
            let mut progressed = false;
            for offset in 0..branches.len() {
                if admitted.len() >= free {
                    break;
                }
                let branch = &branches[(rotation + offset) % branches.len()];
                if let Some(index) = remaining
                    .iter()
                    .position(|record| branch_of(record) == *branch)
                {
                    let record = remaining.remove(index);
                    admitted.push(record.id);
                    progressed = true;
                }
            }
            if !progressed {
                break;
            }
        }
        (admitted, branches)
    }

    /// Reserves an execution lease for one already-recorded task.
    ///
    /// This is the only way a task starts. It decides and records under one
    /// lock, so the lease it hands out is one the count it read actually had
    /// free: concurrent submissions cannot over-admit.
    ///
    /// A task that is not the next fair candidate stays `queued` with
    /// `execution_capacity` as its wait reason. A task whose deadline passed
    /// while it waited is stopped instead of started.
    pub fn acquire(&self, task_id: &str, now_ms: i64) -> Admission {
        let Ok(_lock) = self.registry.execution_scope_lock() else {
            return Admission::Queued;
        };
        self.arbiter
            .decide(|state| self.acquire_locked(state, task_id, now_ms))
    }

    /// Accepts one new child: validate the bounds, record it, and decide its
    /// lease as a single serialized transaction.
    ///
    /// The three steps have to agree with each other. Reading the counts and
    /// then inserting a record lets two concurrent submissions both see room
    /// for one, and deciding a lease from a count another thread is about to
    /// change hands out a lease that does not exist. Holding the scope lock
    /// across all three makes the record and the reservation describe the same
    /// decision.
    ///
    /// A refusal returns without creating anything: a `queue_full` or
    /// `live_task_limit` answer means the task does not exist, so the caller
    /// cannot later find a half-accepted task.
    pub fn submit_subagent(
        &self,
        description: String,
        agent_type: Option<String>,
        parent_task_id: Option<String>,
        now_ms: i64,
    ) -> Result<Submission, AdmissionRefusal> {
        self.submit_subagent_with_intent(description, agent_type, parent_task_id, now_ms, None)
    }

    pub(crate) fn submit_subagent_with_intent(
        &self,
        description: String,
        agent_type: Option<String>,
        parent_task_id: Option<String>,
        now_ms: i64,
        launch_intent: Option<crate::tasks::QueuedSubagentLaunchIntent>,
    ) -> Result<Submission, AdmissionRefusal> {
        let _lock = self
            .registry
            .execution_scope_lock()
            .map_err(|_| AdmissionRefusal::Unavailable)?;
        self.arbiter.decide(|state| {
            if let Some(refusal) = self.refuses() {
                return Err(refusal);
            }
            let handle = self.registry.create_subagent_with_parent_and_intent(
                description,
                agent_type,
                parent_task_id,
                launch_intent,
            );
            let record = self.registry.get(&handle.id);
            // A child submitted into an already-cancelled tree is recorded as
            // stopping: it is accepted work, but it must never start.
            let cancelled = record.as_ref().is_none_or(|record| {
                record.status != TaskStatus::Queued || record.control.cancel.is_cancelled()
            });
            let admission = if cancelled {
                // This child can never run, so its record is settled in the
                // same decision: a `stopping` or `queued` task would otherwise
                // hold capacity or a queue position for work that never began.
                let _ = self.registry.stop(
                    &handle.id,
                    "the task tree was already cancelled when this task was submitted".to_string(),
                );
                Admission::Queued
            } else {
                self.acquire_locked(state, &handle.id, now_ms)
            };
            Ok(Submission {
                task_id: handle.id,
                admission,
                cancelled,
            })
        })
    }

    fn acquire_locked(&self, state: &mut ArbiterState, task_id: &str, now_ms: i64) -> Admission {
        let Some(record) = self.registry.get(task_id) else {
            return Admission::Queued;
        };
        if record.status != TaskStatus::Queued {
            return if record.status == TaskStatus::Running && !record.control.cancel.is_cancelled()
            {
                Admission::Started
            } else {
                Admission::Queued
            };
        }
        if deadline_expired(&record, now_ms) {
            let _ = self.registry.stop(
                task_id,
                "execution deadline passed before the task started".to_string(),
            );
            self.arbiter.announce();
            return Admission::Queued;
        }
        // The queue bound and the live-task bound gate new submissions, not
        // recorded ones: a task that already has a record was accepted, so a
        // full new-task queue never keeps it from a freed lease.
        let free = self.limits.max_running.saturating_sub(self.running());
        if free == 0 {
            let _ = self
                .registry
                .set_wait_reason(task_id, Some(WaitReason::ExecutionCapacity));
            return Admission::Queued;
        }
        let branch = branch_of(&record);
        let Ok(durable) = self.registry.execution_scope_cursor() else {
            return Admission::Queued;
        };
        let (admitted, _) = self.fair_order(
            durable.as_deref().or(state.last_branch.as_deref()),
            free,
            now_ms,
        );
        if !admitted.iter().any(|id| id == task_id) {
            let _ = self
                .registry
                .set_wait_reason(task_id, Some(WaitReason::ExecutionCapacity));
            return Admission::Queued;
        }
        // Commit the scheduling hint before the lease. A crash between these
        // writes can rotate a branch early, but can never duplicate execution.
        if self.registry.set_execution_scope_cursor(&branch).is_err()
            || self.registry.mark_running(task_id).is_err()
        {
            // Cancelled or closed between submission and this decision.
            return Admission::Queued;
        }
        // The branch that was served starts the next round at the back.
        state.last_branch = Some(branch);
        self.arbiter.announce();
        Admission::Started
    }

    /// Waits until some capacity-relevant fact changes.
    ///
    /// Returns `true` when the caller should re-check admission, `false` on
    /// timeout or cancellation. Waiting holds no lease and starts no work.
    pub fn wait_for_capacity(&self, cancel: Option<&CancelToken>, timeout: Duration) -> bool {
        self.arbiter.wait(cancel, timeout)
    }
}

/// The branch a task belongs to for fair scheduling: its parent, or itself for
/// a task that has no parent.
fn branch_of(record: &TaskRecord) -> String {
    record
        .parent_task_id
        .clone()
        .unwrap_or_else(|| record.id.clone())
}

#[cfg(test)]
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

/// Whether a record's own deadline has passed.
fn deadline_expired(record: &TaskRecord, now_ms: i64) -> bool {
    record
        .deadline_at_ms
        .is_some_and(|deadline| now_ms >= deadline)
}

/// Records why a task is waiting, so the reason survives a restart.
pub fn mark_queued(
    registry: &TaskRegistry,
    task_id: &str,
    reason: WaitReason,
) -> Option<TaskRecord> {
    registry.set_wait_reason(task_id, Some(reason))
}

/// Marks a task as no longer waiting: it holds a lease.
pub fn mark_started(registry: &TaskRegistry, task_id: &str) -> Option<TaskRecord> {
    registry.set_wait_reason(task_id, None)
}

/// Statuses that still occupy the scope.
pub fn occupies_scope(status: TaskStatus) -> bool {
    !matches!(
        status,
        TaskStatus::Stopped | TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_view::TaskView;

    fn scope_with(limits: SubagentLimits) -> (TaskRegistry, SubagentLimits) {
        (
            TaskRegistry::new(format!("scope-{}", uuid::Uuid::new_v4())),
            limits,
        )
    }

    fn small(max_running: usize) -> SubagentLimits {
        SubagentLimits {
            max_running,
            max_queued: 4,
            max_live_tasks: 16,
        }
    }

    #[test]
    fn capacity_defaults_match_the_product_decision() {
        let limits = SubagentLimits::default();
        assert_eq!(limits.max_running, 32);
        assert_eq!(limits.max_queued, 256);
        assert_eq!(limits.max_live_tasks, 512);
    }

    #[test]
    fn a_queued_child_does_not_hold_a_lease() {
        let (registry, limits) = scope_with(small(2));
        let scope = ExecutionScope::new(&registry, limits);
        registry.create_subagent("one".to_string(), None);

        assert_eq!(scope.running(), 0);
        assert_eq!(scope.queued().len(), 1);
        assert_eq!(scope.live(), 1);
    }

    #[test]
    fn execution_is_admitted_up_to_the_limit_and_the_rest_queue() {
        let (registry, limits) = scope_with(small(2));
        let scope = ExecutionScope::new(&registry, limits);

        let first = registry.create_subagent("one".to_string(), None);
        let second = registry.create_subagent("two".to_string(), None);
        let third = registry.create_subagent("three".to_string(), None);

        assert_eq!(scope.classify(&first.id), Some(Admission::Started));
        registry.mark_running(&first.id).expect("running");
        assert_eq!(scope.classify(&second.id), Some(Admission::Started));
        registry.mark_running(&second.id).expect("running");
        assert_eq!(
            scope.classify(&third.id),
            Some(Admission::Queued),
            "the third submission waits instead of failing"
        );

        assert_eq!(scope.running(), 2);
        assert_eq!(scope.queued().len(), 1);
        assert_eq!(scope.capacity().limits.max_running, 2);
    }

    #[test]
    fn releasing_one_lease_admits_exactly_one_queued_task() {
        let (registry, limits) = scope_with(small(1));
        let scope = ExecutionScope::new(&registry, limits);

        let running = registry.create_subagent("running".to_string(), None);
        registry.mark_running(&running.id).expect("running");
        let first = registry.create_subagent("first".to_string(), None);
        let second = registry.create_subagent("second".to_string(), None);

        assert!(
            scope.ready_to_start(now_ms()).is_empty(),
            "no lease is free yet"
        );

        registry
            .complete(&running.id, "done".to_string())
            .expect("complete");

        let ready = scope.ready_to_start(now_ms());
        assert_eq!(
            ready,
            vec![first.id.clone()],
            "one freed lease admits one task, not the whole queue"
        );

        // After that task takes the lease, nothing else may start.
        registry.mark_running(&first.id).expect("running");
        assert!(scope.ready_to_start(now_ms()).is_empty());
        assert_eq!(
            scope
                .queued()
                .iter()
                .map(|record| record.id.clone())
                .collect::<Vec<_>>(),
            vec![second.id.clone()]
        );
    }

    #[test]
    fn a_stopping_task_keeps_its_lease_until_execution_is_silent() {
        let (registry, limits) = scope_with(small(1));
        let scope = ExecutionScope::new(&registry, limits);

        let running = registry.create_subagent("running".to_string(), None);
        registry.mark_running(&running.id).expect("running");
        let waiting = registry.create_subagent("waiting".to_string(), None);

        registry.request_stop(&running.id).expect("request stop");

        assert_eq!(
            scope.running(),
            1,
            "a cancellation request is not proof that execution stopped"
        );
        assert!(
            scope.ready_to_start(now_ms()).is_empty(),
            "a stopping task must not have its lease handed to another task"
        );

        registry
            .stop(&running.id, "stopped".to_string())
            .expect("stop");
        assert_eq!(scope.ready_to_start(now_ms()), vec![waiting.id]);
    }

    #[test]
    fn a_full_queue_refuses_instead_of_accepting_unbounded_work() {
        let (registry, limits) = scope_with(SubagentLimits {
            max_running: 1,
            max_queued: 2,
            max_live_tasks: 64,
        });
        let scope = ExecutionScope::new(&registry, limits);

        let running = registry.create_subagent("running".to_string(), None);
        registry.mark_running(&running.id).expect("running");
        registry.create_subagent("queued-one".to_string(), None);
        registry.create_subagent("queued-two".to_string(), None);

        let refusal = scope.refuses().expect("queue is full");
        assert_eq!(
            refusal,
            AdmissionRefusal::QueueFull {
                queued: 2,
                limit: 2
            }
        );
        let message = refusal.message();
        assert!(message.contains("2/2"));
        assert!(message.contains("was not accepted"));
        assert!(
            message.contains("task_stop"),
            "a refusal must point at what still works: {message}"
        );
    }

    #[test]
    fn the_live_task_limit_bounds_waiting_children_too() {
        let (registry, limits) = scope_with(SubagentLimits {
            max_running: 1,
            max_queued: 64,
            max_live_tasks: 3,
        });
        let scope = ExecutionScope::new(&registry, limits);

        for index in 0..3 {
            let child = registry.create_subagent(format!("child-{index}"), None);
            if index == 0 {
                registry.mark_running(&child.id).expect("running");
            }
        }

        assert_eq!(
            scope.refuses(),
            Some(AdmissionRefusal::LiveTaskLimit { live: 3, limit: 3 }),
            "a waiting child is neither running nor queued and still needs a bound"
        );
    }

    #[test]
    fn branches_rotate_so_one_parent_cannot_monopolise_the_scope() {
        let (registry, limits) = scope_with(small(2));
        let scope = ExecutionScope::new(&registry, limits);

        let parent_a = registry.create_subagent("parent-a".to_string(), None);
        let parent_b = registry.create_subagent("parent-b".to_string(), None);
        registry.mark_running(&parent_a.id).expect("running");
        registry.mark_running(&parent_b.id).expect("running");
        // A keeps submitting; the scope has no free lease.
        let a1 =
            registry.create_subagent_with_parent("a1".to_string(), None, Some(parent_a.id.clone()));
        let a2 =
            registry.create_subagent_with_parent("a2".to_string(), None, Some(parent_a.id.clone()));
        let b1 =
            registry.create_subagent_with_parent("b1".to_string(), None, Some(parent_b.id.clone()));

        // Two leases free up at once.
        registry.complete(&parent_a.id, "done".to_string()).unwrap();
        registry.complete(&parent_b.id, "done".to_string()).unwrap();

        let ready = scope.ready_to_start(now_ms());
        assert_eq!(ready.len(), 2);
        assert!(ready.contains(&a1.id), "{ready:?}");
        assert!(
            ready.contains(&b1.id),
            "branch B must be admitted before A's second task: {ready:?}"
        );
        assert!(!ready.contains(&a2.id), "{ready:?}");
    }

    /// The plan's acceptance scenario 1, at the scope level.
    ///
    /// 100 submissions against a 32-lease scope must produce exactly 32 running
    /// and 68 waiting; releasing one barrier admits exactly one more; and the
    /// whole set eventually finishes. A barrier stands in for real work: the
    /// scope counts leases, not process completion.
    #[test]
    fn one_hundred_submissions_produce_thirty_two_running_and_sixty_eight_queued() {
        let limits = SubagentLimits {
            max_running: 32,
            max_queued: 256,
            max_live_tasks: 512,
        };
        let (registry, limits) = scope_with(limits);
        let scope = ExecutionScope::new(&registry, limits);

        let submitted = (0..100)
            .map(|index| registry.create_subagent(format!("task-{index}"), None))
            .collect::<Vec<_>>();

        // Admit as far as the scope allows, exactly as a scheduler would.
        let mut admitted = 0usize;
        loop {
            let ready = scope.ready_to_start(0);
            if ready.is_empty() {
                break;
            }
            for task_id in ready {
                registry.mark_running(&task_id).expect("running");
                admitted += 1;
            }
        }
        assert_eq!(admitted, 32, "a 32-lease scope admits exactly 32 tasks");

        let capacity = scope.capacity();
        assert_eq!(capacity.running, 32);
        assert_eq!(capacity.queued, 68);
        assert_eq!(capacity.live, 100);

        // Releasing one lease admits exactly one more.
        registry
            .complete(&submitted[0].id, "done".to_string())
            .expect("complete");
        let ready = scope.ready_to_start(0);
        assert_eq!(
            ready.len(),
            1,
            "one released lease admits one task, not the rest of the queue"
        );
        registry.mark_running(&ready[0]).expect("running");
        assert_eq!(scope.capacity().running, 32);
        assert_eq!(scope.capacity().queued, 67);

        // Draining the whole scope finishes every task: each round settles the
        // running set and admits whatever the freed leases allow.
        for _ in 0..submitted.len() {
            let running = scope.running_for_test();
            for task_id in &running {
                registry
                    .complete(task_id, "done".to_string())
                    .expect("complete");
            }
            for task_id in scope.ready_to_start(0) {
                registry.mark_running(&task_id).expect("running");
            }
        }
        assert_eq!(
            scope.capacity().live,
            0,
            "every task reached a terminal state: {:?}",
            scope.capacity()
        );
        assert_eq!(scope.capacity().running, 0);
        assert_eq!(scope.capacity().queued, 0);
    }

    /// The plan's acceptance scenario 3, at the scope level.
    ///
    /// A full scope of parents that each block on a descendant must still make
    /// progress without raising the limit. A parent that is blocked on its
    /// child has given back its lease, so the child is admitted in exactly the
    /// capacity the parent returned.
    #[test]
    fn a_blocked_parent_gives_its_lease_to_the_child_it_waits_for() {
        let limits = SubagentLimits {
            max_running: 4,
            max_queued: 64,
            max_live_tasks: 512,
        };
        let (registry, limits) = scope_with(limits);
        let scope = ExecutionScope::new(&registry, limits);

        // Four parents take the whole scope.
        let parents = (0..4)
            .map(|index| registry.create_subagent(format!("parent-{index}"), None))
            .collect::<Vec<_>>();
        for parent in &parents {
            registry.mark_running(&parent.id).expect("running");
        }
        assert_eq!(scope.running(), 4, "the scope starts full");

        // Each parent submits a descendant and blocks on it. Submitting alone
        // does not free anything: the parent still holds its lease until it is
        // actually waiting.
        let mut children = Vec::new();
        for parent in &parents {
            let child = registry.create_subagent_with_parent(
                "child".to_string(),
                None,
                Some(parent.id.clone()),
            );
            assert!(registry.requeue_for_resume(&parent.id, WaitReason::Dependency));
            let view = TaskView::lookup(&registry, &parent.id).expect("view");
            assert_eq!(
                view.execution,
                ExecutionState::WaitingChildren,
                "a parent blocked on live children must not hold an execution lease"
            );
            children.push(child);
        }

        // Every child is now admissible at the same limit, because every
        // parent gave its lease back.
        assert_eq!(
            scope.running(),
            0,
            "only the leases actually in use count: {:?}",
            scope.capacity()
        );
        let ready = scope.ready_to_start(now_ms());
        assert_eq!(
            ready.len(),
            4,
            "each blocked parent's lease admits the child it waits for: {ready:?}"
        );
        for task_id in &ready {
            registry.mark_running(task_id).expect("child runs");
        }
        assert_eq!(scope.running(), 4);
        assert_eq!(scope.queued().len(), 4);

        // Every child finishes; the parents are no longer blocked and become
        // schedulable again at the same limit.
        for child in &children {
            registry
                .complete(&child.id, "done".to_string())
                .expect("child completes");
        }
        for parent in &parents {
            let view = TaskView::lookup(&registry, &parent.id).expect("view");
            assert!(
                view.execution.is_schedulable(),
                "a parent whose children settled must be schedulable again: {view:?}"
            );
        }
    }

    #[test]
    fn a_deadline_that_passed_while_queued_stops_the_task_instead_of_starting_it() {
        let (registry, limits) = scope_with(small(1));
        let scope = ExecutionScope::new(&registry, limits);

        let running = registry.create_subagent("running".to_string(), None);
        registry.mark_running(&running.id).expect("running");
        let queued = registry.create_subagent("queued".to_string(), None);
        registry
            .set_deadline_at(&queued.id, 1, "caller deadline_ms")
            .expect("deadline");

        // The lease frees, but the queued task is already out of time.
        registry
            .complete(&running.id, "done".to_string())
            .expect("complete");
        assert!(
            scope.ready_to_start(now_ms()).is_empty(),
            "a task past its deadline must never be started"
        );

        let stopped = scope.stop_expired(now_ms());
        assert_eq!(stopped, vec![queued.id.clone()]);
        assert_eq!(
            registry.get(&queued.id).map(|record| record.status),
            Some(TaskStatus::Stopped),
            "an expired deadline stops the task rather than leaving it waiting"
        );
    }

    #[test]
    fn a_deadline_in_the_future_does_not_block_scheduling() {
        let (registry, limits) = scope_with(small(1));
        let scope = ExecutionScope::new(&registry, limits);

        let queued = registry.create_subagent("queued".to_string(), None);
        registry
            .set_deadline_at(&queued.id, now_ms().saturating_add(600_000), "later")
            .expect("deadline");

        assert_eq!(scope.ready_to_start(now_ms()), vec![queued.id.clone()]);
        assert!(scope.stop_expired(now_ms()).is_empty());
    }

    #[test]
    fn a_wait_reason_survives_reload() {
        let (registry, _limits) = scope_with(small(1));
        let running = registry.create_subagent("running".to_string(), None);
        registry.mark_running(&running.id).expect("running");
        let queued = registry.create_subagent("queued".to_string(), None);

        mark_queued(&registry, &queued.id, WaitReason::ExecutionCapacity);
        let view = TaskView::lookup(&registry, &queued.id).expect("view");
        assert_eq!(view.wait_reason, Some(WaitReason::ExecutionCapacity));

        mark_started(&registry, &queued.id);
        let view = TaskView::lookup(&registry, &queued.id).expect("view");
        assert_eq!(view.wait_reason, None);
    }

    /// The submission transaction: the bound is checked, the record is
    /// created, and the lease is decided under one lock.
    #[test]
    fn submitting_beyond_the_running_limit_queues_the_rest_with_a_reason() {
        let (registry, limits) = scope_with(small(2));
        let scope = ExecutionScope::new(&registry, limits);

        let mut submissions = Vec::new();
        for index in 0..5 {
            submissions.push(
                scope
                    .submit_subagent(format!("child-{index}"), None, None, now_ms())
                    .expect("accepted"),
            );
        }

        let started = submissions
            .iter()
            .filter(|submission| submission.admission == Admission::Started)
            .count();
        assert_eq!(started, 2, "the limit is a lease count, not a hint");
        assert_eq!(scope.running(), 2);
        assert_eq!(scope.queued().len(), 3);
        for submission in &submissions {
            let record = registry.get(&submission.task_id).expect("record");
            match submission.admission {
                Admission::Started => assert_eq!(record.status, TaskStatus::Running),
                Admission::Queued => {
                    assert_eq!(record.status, TaskStatus::Queued);
                    assert_eq!(
                        record.wait_reason,
                        Some(WaitReason::ExecutionCapacity),
                        "a queued task carries the reason it waits"
                    );
                }
            }
        }
    }

    /// A child submitted into a tree that is already cancelled is settled, not
    /// left holding a lease for work that never ran.
    #[test]
    fn a_child_submitted_into_a_cancelled_tree_does_not_hold_a_lease() {
        let (registry, limits) = scope_with(small(2));
        let scope = ExecutionScope::new(&registry, limits);

        let root = registry.create_main_session("root".to_string());
        registry
            .request_stop(&root.id)
            .expect("the tree is cancelled");
        let submission = scope
            .submit_subagent(
                "too late".to_string(),
                None,
                Some(root.id.clone()),
                now_ms(),
            )
            .expect("accepted into a cancelled tree");
        assert!(submission.cancelled, "nothing can start here");

        let record = registry.get(&submission.task_id).expect("record");
        assert!(
            !TaskStatus::is_active(record.status),
            "a cancelled submission is settled at once: {:?}",
            record.status
        );
        assert_eq!(scope.running(), 0);
        assert_eq!(scope.live(), 0);
    }

    /// Acceptance scenario 5 at the scope level: a boundary refuses without
    /// creating anything, so a refused submission cannot be mistaken for work
    /// that exists.
    #[test]
    fn a_submission_past_the_queue_bound_creates_no_record() {
        let (registry, limits) = scope_with(SubagentLimits {
            max_running: 1,
            max_queued: 1,
            max_live_tasks: 64,
        });
        let scope = ExecutionScope::new(&registry, limits);

        let running = scope
            .submit_subagent("running".to_string(), None, None, now_ms())
            .expect("accepted");
        assert_eq!(running.admission, Admission::Started);
        let queued = scope
            .submit_subagent("queued".to_string(), None, None, now_ms())
            .expect("accepted");
        assert_eq!(queued.admission, Admission::Queued);
        let before = registry.list().len();

        let refusal = scope
            .submit_subagent("refused".to_string(), None, None, now_ms())
            .expect_err("the queue bound is a real boundary");
        assert!(matches!(refusal, AdmissionRefusal::QueueFull { .. }));
        assert_eq!(
            registry.list().len(),
            before,
            "a refusal must not leave a half-accepted task behind"
        );
    }

    /// Accepted work is never blocked by the bound on *new* work: the queue
    /// bound gates submissions, not the tasks already in the queue.
    #[test]
    fn a_full_new_task_queue_does_not_stop_an_accepted_task_from_starting() {
        let (registry, limits) = scope_with(SubagentLimits {
            max_running: 1,
            max_queued: 1,
            max_live_tasks: 64,
        });
        let scope = ExecutionScope::new(&registry, limits);

        let running = scope
            .submit_subagent("running".to_string(), None, None, now_ms())
            .expect("accepted");
        let queued = scope
            .submit_subagent("queued".to_string(), None, None, now_ms())
            .expect("accepted");
        assert_eq!(queued.admission, Admission::Queued);
        assert!(scope.refuses().is_some(), "the new-task queue is full");

        registry
            .complete(&running.task_id, "done".to_string())
            .expect("complete");
        assert_eq!(
            scope.acquire(&queued.task_id, now_ms()),
            Admission::Started,
            "a freed lease belongs to the task that was already accepted"
        );
    }

    /// Waking from a wait is not a way around the lease: the parent goes back
    /// into the queue and becomes running only when it is admitted.
    #[test]
    fn a_woken_parent_waits_for_a_lease_before_it_counts_as_running() {
        let (registry, limits) = scope_with(small(1));
        let scope = ExecutionScope::new(&registry, limits);

        let occupant = registry.create_subagent("occupant".to_string(), None);
        registry.mark_running(&occupant.id).expect("running");
        let parent = registry.create_subagent("parent".to_string(), None);
        registry.mark_running(&parent.id).expect("running");
        registry
            .requeue_for_resume(&parent.id, WaitReason::Dependency)
            .then_some(())
            .expect("the parent returns to the queue");
        let child = registry.create_subagent_with_parent(
            "child".to_string(),
            None,
            Some(parent.id.clone()),
        );

        // The child takes the freed lease; the parent does not.
        registry
            .complete(&occupant.id, "done".to_string())
            .expect("complete");
        assert_eq!(
            scope.acquire(&child.id, now_ms()),
            Admission::Started,
            "the child of a waiting parent is admitted"
        );
        assert_eq!(scope.acquire(&parent.id, now_ms()), Admission::Queued);
        assert_eq!(scope.running(), 1);

        registry
            .complete(&child.id, "done".to_string())
            .expect("complete");
        assert_eq!(scope.acquire(&parent.id, now_ms()), Admission::Started);
        assert_eq!(scope.running(), 1, "the parent resumed inside the limit");
    }

    /// Fairness with one free lease at a time: the branch that was just served
    /// waits its turn instead of taking the next lease as well.
    #[test]
    fn one_free_lease_rotates_between_branches() {
        let (registry, limits) = scope_with(SubagentLimits {
            max_running: 3,
            max_queued: 8,
            max_live_tasks: 32,
        });
        let scope = ExecutionScope::new(&registry, limits);

        // Two branches, each held open by a child that is already running, so
        // the parents stay blocked on their children for the whole test.
        let parent_a = registry.create_subagent("parent-a".to_string(), None);
        registry.mark_running(&parent_a.id).expect("running");
        let anchor_a = registry.create_subagent_with_parent(
            "anchor-a".to_string(),
            None,
            Some(parent_a.id.clone()),
        );
        registry.mark_running(&anchor_a.id).expect("running");
        let parent_b = registry.create_subagent("parent-b".to_string(), None);
        registry.mark_running(&parent_b.id).expect("running");
        let anchor_b = registry.create_subagent_with_parent(
            "anchor-b".to_string(),
            None,
            Some(parent_b.id.clone()),
        );
        registry.mark_running(&anchor_b.id).expect("running");

        let a1 =
            registry.create_subagent_with_parent("a1".to_string(), None, Some(parent_a.id.clone()));
        let a2 =
            registry.create_subagent_with_parent("a2".to_string(), None, Some(parent_a.id.clone()));
        let b1 =
            registry.create_subagent_with_parent("b1".to_string(), None, Some(parent_b.id.clone()));
        assert!(registry.requeue_for_resume(&parent_a.id, WaitReason::Dependency));
        assert!(registry.requeue_for_resume(&parent_b.id, WaitReason::Dependency));
        // Two of the three leases are held by the anchors; one is free.
        assert_eq!(scope.running(), 2);

        assert_eq!(scope.acquire(&a1.id, now_ms()), Admission::Started);

        registry
            .complete(&a1.id, "done".to_string())
            .expect("complete");
        assert_eq!(
            scope.acquire(&a2.id, now_ms()),
            Admission::Queued,
            "branch A was just served; B is next"
        );
        assert_eq!(scope.acquire(&b1.id, now_ms()), Admission::Started);

        registry
            .complete(&b1.id, "done".to_string())
            .expect("complete");
        assert_eq!(scope.acquire(&a2.id, now_ms()), Admission::Started);
        assert_eq!(scope.running(), 3, "the limit still holds");
    }

    /// Acceptance scenario 3 at the scope level, at full width: every running
    /// task is a parent waiting for a descendant, and the tree still advances.
    #[test]
    fn thirty_two_waiting_parents_keep_the_scope_full_without_exceeding_it() {
        let limits = SubagentLimits {
            max_running: 32,
            max_queued: 256,
            max_live_tasks: 512,
        };
        let (registry, limits) = scope_with(limits);
        let scope = ExecutionScope::new(&registry, limits);

        let mut pairs = Vec::new();
        for index in 0..32 {
            let parent = registry.create_subagent(format!("parent-{index}"), None);
            registry.mark_running(&parent.id).expect("running");
            let child = registry.create_subagent_with_parent(
                format!("child-{index}"),
                None,
                Some(parent.id.clone()),
            );
            // A parent that blocks on its child returns to the queue first;
            // that is the state the scope sees while it waits.
            registry
                .requeue_for_resume(&parent.id, WaitReason::Dependency)
                .then_some(())
                .expect("parent waits");
            pairs.push((parent.id, child.id));
        }

        for (_, child) in &pairs {
            assert_eq!(
                scope.acquire(child, now_ms()),
                Admission::Started,
                "each waiting parent gave its lease to the child it waits for"
            );
        }
        assert_eq!(scope.running(), 32);
        assert_eq!(
            scope.queued().len(),
            32,
            "the parents are queued for resume, not running"
        );
        assert_eq!(scope.live(), 64, "parents and children all still exist");

        // As each child settles, exactly one parent takes the freed lease.
        for (parent, child) in &pairs {
            registry
                .complete(child, "done".to_string())
                .expect("child completes");
            assert_eq!(
                scope.acquire(parent, now_ms()),
                Admission::Started,
                "the parent resumes in the capacity its child returned"
            );
            assert!(scope.running() <= 32, "the limit holds through the wake");
        }
        assert_eq!(scope.capacity().queued, 0);
    }

    /// Two threads submitting at the same moment must not both take the last
    /// lease: the decision and the record it decides on share one lock.
    #[test]
    fn concurrent_acquisitions_never_exceed_the_running_limit() {
        let limits = SubagentLimits {
            max_running: 8,
            max_queued: 256,
            max_live_tasks: 512,
        };
        let (registry, limits) = scope_with(limits);
        let scope = ExecutionScope::new(&registry, limits);
        let mut submissions = Vec::new();
        for index in 0..40 {
            submissions.push(
                scope
                    .submit_subagent(format!("child-{index}"), None, None, now_ms())
                    .expect("accepted")
                    .task_id,
            );
        }

        let mut started = 0usize;
        let racing_scope = &scope;
        let racing_registry = &registry;
        std::thread::scope(|threads| {
            let handles = submissions
                .iter()
                .map(|task_id| {
                    let task_id = task_id.clone();
                    threads.spawn(move || {
                        // Racing callers, each trying to take a lease for its
                        // own task.
                        let _ = racing_scope.acquire(&task_id, now_ms());
                        racing_registry.get(&task_id).map(|record| record.status)
                    })
                })
                .collect::<Vec<_>>();
            for handle in handles {
                if handle.join().expect("thread") == Some(TaskStatus::Running) {
                    started += 1;
                }
            }
        });

        assert_eq!(started, 8, "eight leases, eight runners, whatever the race");
        assert_eq!(scope.running(), 8);
        assert_eq!(scope.queued().len(), 32);
    }
    #[test]
    fn a_running_parent_with_background_children_keeps_its_lease() {
        let (registry, limits) = scope_with(small(1));
        let scope = registry.execution_scope(&limits);
        let parent = registry.create_subagent("parent".into(), None);
        registry.mark_running(&parent.id).unwrap();
        registry.create_subagent_with_parent("background".into(), None, Some(parent.id.clone()));
        assert_eq!(scope.running(), 1);
        assert!(
            TaskView::lookup(&registry, &parent.id)
                .unwrap()
                .execution
                .holds_execution_lease()
        );
        assert!(scope.ready_to_start(now_ms()).is_empty());
    }

    #[test]
    fn dispatcher_runs_only_thirty_two_of_one_hundred_accepted_jobs() {
        let registry = TaskRegistry::new("dispatcher-capacity".into());
        let limits = SubagentLimits::default();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let mut releases = Vec::new();
        let mut ids = Vec::new();
        for index in 0..100 {
            let submission = registry
                .execution_scope(&limits)
                .submit_subagent(format!("job {index}"), None, None, now_ms())
                .unwrap();
            let id = submission.task_id;
            let (release, wait) = std::sync::mpsc::channel();
            releases.push(release);
            ids.push(id.clone());
            let worker_registry = registry.clone();
            let started = started_tx.clone();
            registry
                .scope_arbiter()
                .enqueue(registry.clone(), id.clone(), limits, move || {
                    let _ = started.send(index);
                    let _ = wait.recv();
                    worker_registry.complete(&id, "done".into()).unwrap();
                })
                .unwrap();
        }
        let mut running_indices = Vec::new();
        for _ in 0..32 {
            running_indices.push(started_rx.recv_timeout(Duration::from_secs(5)).unwrap());
        }
        assert!(started_rx.recv_timeout(Duration::from_millis(100)).is_err());
        let capacity = registry.execution_scope(&limits).capacity();
        assert_eq!((capacity.running, capacity.queued), (32, 68));
        releases[running_indices[0]].send(()).unwrap();
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("exactly one replacement");
        assert!(started_rx.recv_timeout(Duration::from_millis(100)).is_err());
        for release in releases {
            let _ = release.send(());
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while registry.execution_scope(&limits).live() != 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "accepted queue did not drain"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            ids.iter()
                .filter(|id| registry.get(id).unwrap().status == TaskStatus::Completed)
                .count(),
            100
        );
    }

    #[test]
    fn running_deadline_requests_stop_without_releasing_its_lease() {
        let registry = TaskRegistry::new("running-deadline".into());
        let limits = SubagentLimits::default();
        let task = registry
            .execution_scope(&limits)
            .submit_subagent("running".into(), None, None, now_ms())
            .unwrap();
        registry.set_deadline_at(&task.task_id, now_ms() - 1, "test deadline");
        registry.execution_scope(&limits).stop_expired(now_ms());
        assert_eq!(
            registry.get(&task.task_id).unwrap().status,
            TaskStatus::Stopping
        );
        assert_eq!(registry.execution_scope(&limits).running(), 1);
        assert!(registry.is_cancelled(&task.task_id));
        registry
            .stop(&task.task_id, "worker acknowledged cancellation".into())
            .unwrap();
        assert_eq!(registry.execution_scope(&limits).running(), 0);
    }

    #[test]
    fn a_panicked_dispatch_decision_restores_thread_local_lock_state() {
        let arbiter = ScopeArbiter::new();
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                arbiter.decide(|_| {
                    arbiter.announce();
                    panic!("injected decision failure");
                });
            }))
            .is_err()
        );
        assert!(!IN_DECISION.with(Cell::get));
        assert!(!DECISION_DIRTY.with(Cell::get));
        arbiter.announce();
        assert_eq!(arbiter.decide(|state| state.generation), 2);
    }

    #[test]
    fn panicked_worker_retains_indeterminate_reservation() {
        let registry = TaskRegistry::new("panic-reservation".into());
        let limits = small(1);
        let task = registry
            .execution_scope(&limits)
            .submit_subagent("panic".into(), None, None, now_ms())
            .unwrap();
        registry
            .scope_arbiter()
            .enqueue(registry.clone(), task.task_id.clone(), limits, || {
                panic!("external execution receipt missing")
            })
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !registry
            .get(&task.task_id)
            .unwrap()
            .continuation_indeterminate
        {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        let scope = registry.execution_scope(&limits);
        assert_eq!(scope.running(), 1);
        assert_eq!(scope.classify(&task.task_id), None);
        let next = scope
            .submit_subagent("next".into(), None, None, now_ms())
            .unwrap();
        assert_eq!(next.admission, Admission::Queued);
        registry.stop(&next.task_id, "test cleanup".into()).unwrap();
    }
}
