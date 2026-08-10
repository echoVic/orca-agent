# P1.4 Durable Task Supervision Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give persistent background tasks one durable owner lease, fencing epoch, crash takeover path, and task-wide publication revision.

**Architecture:** `TaskRegistry` becomes the single authority for persistent task ownership. A worker acquires a typed lease, renews it while active, and may mutate its task only through a fenced transaction. Stop remains a durable control request that can revoke a dead worker's epoch. Every durable task mutation publishes one incremented snapshot revision, allowing task readers to ignore stale projections.

**Tech Stack:** Rust workspace, serde JSON task persistence, `ExclusiveFileLock`, Tokio/runtime-host task publication, Unix PTY and Windows Job Object contracts.

---

### Task 1: Define durable lease and publication data

**Files:**
- Modify: `crates/orca-runtime/src/tasks.rs:34-220, 1726-2010, 2036-2140`
- Modify: `crates/orca-core/src/task_types.rs:103-164`
- Test: `crates/orca-runtime/src/tasks.rs:2733-`

- [x] **Step 1: Write failing lease and publication tests.**

```rust
#[test]
fn persistent_task_lease_rejects_second_live_owner_and_publishes_revision() {
    let root = tempfile::tempdir().unwrap();
    let owner = TaskRegistry::new_persistent("session".into(), root.path().into()).unwrap();
    let task = owner.create_subagent("work".into(), None);
    let first = owner.acquire_task_lease(&task.id).unwrap();
    let observer = TaskRegistry::new_persistent_attached("session".into(), root.path().into()).unwrap();

    assert!(matches!(observer.acquire_task_lease(&task.id), Err(TaskLeaseError::Held { .. })));
    owner.mark_running_with_lease(&first, &task.id).unwrap();
    assert_eq!(owner.summary(&task.id).unwrap().publication_revision, 2);
}
```

- [x] **Step 2: Run the focused RED test.**

Run: `cargo test -p orca-runtime tasks::tests::persistent_task_lease_rejects_second_live_owner_and_publishes_revision --lib -- --exact --nocapture`

Expected: compilation failure because `TaskLease`, `TaskLeaseError`, and fenced APIs do not exist.

- [x] **Step 3: Add the model and serde migration.**

```rust
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskLease { task_id: String, owner_id: String, epoch: u64, expires_at_ms: i64 }

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskLeaseError { Held { owner_id: String, expires_at_ms: i64 }, Fenced, NotFound }

// PersistedTaskRecord fields use #[serde(default)] for legacy files.
lease_owner: Option<String>, lease_epoch: u64, lease_expires_at_ms: Option<i64>,
stop_requested: bool, publication_revision: u64,
```

Add `publication_revision` to `BackgroundTaskSummary` as an additive optional
`publicationRevision`, and map it in `task_summary`. Existing projections use
`None` until their source itself has durable task revision data.

- [x] **Step 4: Run the focused GREEN test.**

Run: `cargo test -p orca-runtime tasks::tests::persistent_task_lease_rejects_second_live_owner_and_publishes_revision --lib -- --exact --nocapture`

Expected: PASS.

### Task 2: Make persistent mutations fenced transactions

**Files:**
- Modify: `crates/orca-runtime/src/tasks.rs:1726-2014`
- Test: `crates/orca-runtime/src/tasks.rs:2733-`

- [x] **Step 1: Write the stale-owner RED test.**

```rust
#[test]
fn stale_lease_cannot_replace_takeover_terminal() {
    let (first, second, task, first_lease) = two_persistent_registries_with_expired_lease();
    let takeover = second.acquire_task_lease(&task.id).unwrap();
    second.complete_with_lease(&takeover, &task.id, "new owner".into(), None).unwrap();

    assert!(matches!(first.complete_with_lease(&first_lease, &task.id, "stale".into(), None), Err(TaskLeaseError::Fenced)));
    assert_eq!(second.get(&task.id).unwrap().result.as_deref(), Some("new owner"));
}
```

- [x] **Step 2: Run RED.**

Run: `cargo test -p orca-runtime tasks::tests::stale_lease_cannot_replace_takeover_terminal --lib -- --exact --nocapture`

Expected: compilation failure until fenced completion exists.

- [x] **Step 3: Replace unfenced persistent writes with one lock-scoped mutation primitive.**

```rust
fn mutate_persistent_task<R>(
    &self, id: &str, lease: Option<&TaskLease>, mutate: impl FnOnce(&mut TaskRecord) -> Result<R, TaskLeaseError>,
) -> Result<R, TaskLeaseError> {
    // acquire session lock; reload current record; validate lease; mutate;
    // increment publication_revision; atomically write; refresh local mirror
}
```

Use it for lease acquire/renew/release and all worker-owned progress/terminal
methods. Legacy in-memory registries retain their current path.

- [x] **Step 4: Run GREEN and existing persistence tests.**

Run:
`cargo test -p orca-runtime tasks::tests::stale_lease_cannot_replace_takeover_terminal --lib -- --exact --nocapture`

`cargo test -p orca-runtime tasks::tests::persistent_registry_recovers_interrupted_subagent_task_by_id --lib -- --exact --nocapture`

Expected: both PASS.

### Task 3: Integrate async-worker heartbeat, takeover, stop, and reaper fencing

**Files:**
- Modify: `crates/orca-runtime/src/tasks.rs:1067-1168, 1459-1718, 2267-2315`
- Modify: `crates/orca-runtime/src/subagent_async_worker.rs:99-217, 363-404`
- Test: `crates/orca-runtime/src/tasks.rs:3639-4055`
- Test: `crates/orca-runtime/src/subagent_async_worker.rs:601-`

- [x] **Step 1: Write RED tests for expired takeover and forced-stop fencing.**

```rust
#[test]
fn expired_worker_lease_is_taken_over_and_old_reaper_cannot_fail_it() {
    let (owner, recovery, task, old_lease) = expired_worker_lease_fixture();
    let takeover = recovery.acquire_task_lease(&task.id).unwrap();
    recovery.complete_with_lease(&takeover, &task.id, "recovered".into(), None).unwrap();

    assert!(matches!(owner.fail_with_lease(&old_lease, &task.id, "late reaper".into(), None), Err(TaskLeaseError::Fenced)));
    assert_eq!(recovery.get(&task.id).unwrap().result.as_deref(), Some("recovered"));
}
```

- [x] **Step 2: Run RED.**

Run: `cargo test -p orca-runtime tasks::tests::expired_worker_lease_is_taken_over_and_old_reaper_cannot_fail_it --lib -- --exact --nocapture`

Expected: compilation failure until takeover and fenced reaper behavior exist.

- [x] **Step 3: Acquire/renew in the worker and revoke on forced stop.**

```rust
let lease = task_registry.acquire_task_lease(&agent_id)?;
task_registry.mark_running_with_lease(&lease, &agent_id)?;
// renew around child-agent lifecycle boundaries; return failure when fenced.
```

`request_stop` durably sets `stop_requested`, advances/revokes the old epoch,
and then publishes `Stopped` after it verifies and kills a worker. The reaper
only refreshes its local view; worker fallback publication is always fenced.

- [x] **Step 4: Run GREEN and the existing recovered-worker contracts.**

Run:
`cargo test -p orca-runtime tasks::tests::expired_worker_lease_is_taken_over_and_old_reaper_cannot_fail_it --lib -- --exact --nocapture`

`cargo test -p orca-runtime tasks::tests::request_stop_terminates_verified_recovered_worker_group --lib -- --exact --nocapture`

Expected: PASS on Unix; keep the Windows named-job test enabled for its target.

### Task 4: Publish complete refreshed task snapshots

**Files:**
- Modify: `crates/orca-runtime/src/tasks.rs:765-805, 2318-2351`
- Modify: `crates/orca-runtime/src/runtime_host.rs:37693-37729`
- Test: `crates/orca-runtime/src/tasks.rs:3696-3755`
- Test: `crates/orca-runtime/src/runtime_host.rs`

- [x] **Step 1: Write a RED cross-registry snapshot test.**

```rust
#[test]
fn persistent_list_refreshes_worker_created_tasks_with_monotonic_revisions() {
    let (owner, worker, root) = persistent_registry_pair();
    let child = worker.create_subagent_with_parent("child".into(), None, Some(root.id));

    let published = owner.list();
    assert!(published.iter().any(|task| task.id == child.id));
    assert!(published.windows(2).all(|pair| pair[0].publication_revision <= pair[1].publication_revision || pair[0].id != pair[1].id));
}
```

- [x] **Step 2: Run RED.**

Run: `cargo test -p orca-runtime tasks::tests::persistent_list_refreshes_worker_created_tasks_with_monotonic_revisions --lib -- --exact --nocapture`

Expected: FAIL because `list` reads only the local map and summaries have no revision.

- [x] **Step 3: Refresh before persistent list and emit only durable summaries.**

`list` must merge the complete session file before constructing summaries.
`emit_task_status_update` continues to emit its established event name but uses
the refreshed summary returned after the durable mutation.

- [x] **Step 4: Run GREEN.**

Run: `cargo test -p orca-runtime tasks::tests::persistent_list_refreshes_worker_created_tasks_with_monotonic_revisions --lib -- --exact --nocapture`

Expected: PASS.

### Task 5: Document, verify, review, and commit the release slice

**Files:**
- Modify: `docs/production-roadmap.md`
- Modify: `docs/superpowers/specs/2026-08-10-p1-4-task-supervision.md`
- Modify: `docs/superpowers/plans/2026-08-10-p1-4-task-supervision.md`

- [x] **Step 1: Update the roadmap with implemented ownership, compatibility, and the remaining next slice.**

- [ ] **Step 2: Run focused and shared-runtime verification.**

Run:
`cargo test -p orca-runtime tasks::tests --lib --locked`

`cargo test -p orca-runtime subagent_async_worker --lib --locked`

`cargo test -p orca-runtime --test runtime_host --locked`

`cargo test -p orca-tui --lib --locked`

`cargo fmt --all -- --check`

`git diff --check`

Expected: all focused suites pass and the diff has no whitespace errors.

- [ ] **Step 3: Run full lifecycle gates after rebasing the worktree on the current `origin/main`.**

Run:
`git fetch origin --prune && git rebase origin/main`

`cargo test --workspace --locked`

`cargo clippy --workspace --all-targets --all-features --locked -- -D warnings`

Expected: workspace gates pass; any unrelated baseline failure is recorded with
fresh output and does not become slice evidence.

- [ ] **Step 4: Review the diff and create one semantic commit.**

Review for stale commits, second task authority, owner-id leakage in external
protocols, and accidental task-schema breakage. Commit only the P1.4 source,
tests, spec, plan, and roadmap changes:

`git commit -m "feat(runtime): supervise persistent task ownership"`
