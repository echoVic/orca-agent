# Runtime Legacy Active Task Adoption Implementation Plan

> **For Codex:** Execute this plan with RED/GREEN discipline. Do not fabricate
> resumability, broaden eligibility beyond exact `Running` main-session rows,
> add a TUI registry fallback, or integrate before every listed gate passes.

**Goal:** Adopt safe pre-surface registry-only running main-session rows into
durable runtime operation/background ownership, then truthfully terminalize
their unavailable live capsules through the existing cold-recovery path.

**Architecture:** `TaskRegistry` issues an opaque canonical receipt while
holding the persistent session lock. `RuntimeCommitCoordinator` admits one
exact five-event operation/task/transfer group per missing receipt row. Recorded
startup inserts those groups before its existing operation-recovery scan; the
native cold-recovery and terminal-task reconciliation paths then converge each
row to one stopped surface task and legacy mirror. The TUI remains a surface
projection consumer only.

**Tech Stack:** Rust, serde/serde_json, SHA-256, existing JSONL surface
ledger/reducer/commit coordinator, Tokio runtime host, ratatui projection,
Node contract validators, and Cargo tests.

---

## Task 1: Add the locked active-adoption receipt owner

**Files:**
- Modify: `crates/orca-runtime/src/tasks.rs:96-255, 464-665, 2518-2570, 3218-3245`
- Test: `crates/orca-runtime/src/tasks.rs:3380-3650`

- [x] **Step 1: Write the receipt filtering RED test.**

Add the exact test
`tasks::tests::persistent_active_main_session_adoption_receipt_filters_and_sorts_records`.
Create persistent rows covering:

- two valid running main-session rows inserted in reverse task-id order;
- queued, paused, stopping, approval-required, failed, and terminal rows;
- workflow/subagent/shell/monitor rows;
- missing start time, present completion time, result, error, or tool state;
- pending tool, pending approval response, and pending provider response;
- worker PID, lease owner/expiry, and `stop_requested` states;
- a durable typed-provider outcome; and
- an id that cannot parse as `SurfaceTaskId`.

Assert that the receipt exposes only the two valid rows, sorted by id, with
exact presentation fields, source publication revisions, positive horizon,
session id, and deterministic digest. Assert process-local registries return
`None`.

- [x] **Step 2: Run RED.**

```bash
cargo test -p orca-runtime tasks::tests::persistent_active_main_session_adoption_receipt_filters_and_sorts_records --lib --locked -- --exact --test-threads=1
```

Expected: FAIL because the active-adoption receipt API does not exist.

- [x] **Step 3: Implement the opaque receipt and outcome-file lock boundary.**

Add crate-private `LegacyActiveTaskAdoptionRecord` and
`LegacyActiveTaskAdoptionReceipt` types with private fields. Expose only narrow
immutable queries required by runtime-host mapping and commit authorization.
The canonical record includes task id, description, creation/start timestamps,
partial usage, retry count, truncation, and publication revision. It does not
contain task control, cancellation, worker, lease, pending continuation, or
mutable record state.

Add:

```rust
pub(crate) fn with_active_main_session_adoption<R>(
    &self,
    adopt: impl FnOnce(&LegacyActiveTaskAdoptionReceipt) -> R,
) -> Result<Option<R>, String>
```

For persistent registries, acquire the session lock, reload `tasks.json` and
the typed-provider-outcome file, apply the exact Spec eligibility, sort by id,
compute the positive publication horizon and canonical digest, and invoke the
callback before releasing the lock. Return `Ok(None)` for process-local
registries. A read error is an error.

Make typed-provider-outcome reads/writes use the same session lock. Split
locked and `_unlocked` helpers so the receipt can read while already holding
the lock without recursive acquisition. Preserve the existing map-then-file
write order and do not introduce a session-lock-to-map-lock path.

- [x] **Step 4: Run GREEN and persistence regressions.**

```bash
cargo test -p orca-runtime tasks::tests::persistent_active_main_session_adoption_receipt_filters_and_sorts_records --lib --locked -- --exact --test-threads=1
cargo test -p orca-runtime tasks::tests::persistent_typed_provider_outcome_round_trips_budget_terminal --lib --locked -- --exact --test-threads=1
cargo test -p orca-runtime tasks::tests::persistent_terminal_reconciliation_receipt_filters_and_sorts_records --lib --locked -- --exact --test-threads=1
cargo test -p orca-runtime tasks::tests::stale_worker_exit_fence_cannot_overwrite_takeover_terminal --lib --locked -- --exact --test-threads=1
```

Expected: PASS.

## Task 2: Add receipt-only active-adoption commit authority

**Files:**
- Modify: `crates/orca-runtime/src/runtime_surface/commit.rs:1-20, 621-760, 1364-1390, 2940-3020, 4360-4460`
- Test: `crates/orca-runtime/src/runtime_surface/commit.rs:10120-10450`

- [x] **Step 1: Write fresh-authority RED tests.**

Add exact tests:

- `active_task_receipt_authorizes_exact_operation_transfer_batch`;
- `active_task_receipt_rejects_substitution_omission_and_fence_mismatch`;
- `active_task_receipt_rejects_ambiguous_existing_operation_lineage`;
  and
- `actor_permit_cannot_commit_active_task_adoption_batch`.

Obtain a real receipt from a persistent registry. Build the canonical five
events from a reducer snapshot with current settings/owner epoch. The positive
test asserts a committed operation, background operation, and running task.
Negatives independently change task presentation, remove a receipt row, break
operation/task/background identity, use replayable input, change settings or
capability fingerprints, add an event, and attempt the same batch through
`commit_actor_batch`.

- [x] **Step 2: Run RED.**

```bash
cargo test -p orca-runtime runtime_surface::commit::tests::active_task_receipt_ --lib --locked -- --test-threads=1
cargo test -p orca-runtime runtime_surface::commit::tests::actor_permit_cannot_commit_active_task_adoption_batch --lib --locked -- --exact --test-threads=1
```

Expected: FAIL because the receipt authority does not exist.

- [x] **Step 3: Implement the dedicated fresh authority.**

Add private `BatchCommitAuthority::ActiveTaskAdoption` and a crate-private
coordinator method accepting the receipt and batch. Validate recorded thread
identity, owner epoch, receipt validity, event count and five-event grouping,
sorted unique tasks, exact receipt-derived task facts, current settings/policy,
canonical non-replayable missing capsule, empty capabilities, matching
fingerprints/witness, fresh operation identities, and exact one-to-one fences.

Fresh authorization must require all missing receipt records and no ineligible
or already-existing record. It may not modify current task/operation state.
Keep terminal `TaskPatch::Reconciled` authority unchanged.

- [x] **Step 4: Write and implement prepared-batch recovery RED/GREEN.**

Add
`prepared_active_task_adoption_recovers_only_canonical_shape`. Feed recovered
prepared batches containing the valid shape, replayable input, missing task,
wrong fence, and non-main task. Observe RED, then add a private recovered
authority selected only for the canonical event shape. Do not expose it as a
fresh public commit method.

```bash
cargo test -p orca-runtime runtime_surface::commit::tests::prepared_active_task_adoption_recovers_only_canonical_shape --lib --locked -- --exact --test-threads=1
```

Expected: PASS after implementation.

- [x] **Step 5: Run complete commit regressions.**

```bash
cargo test -p orca-runtime runtime_surface::commit::tests::active_task_receipt_ --lib --locked -- --test-threads=1
cargo test -p orca-runtime runtime_surface::commit::tests::actor_permit_cannot_commit_active_task_adoption_batch --lib --locked -- --exact --test-threads=1
cargo test -p orca-runtime runtime_surface::commit::tests::prepared_active_task_adoption_recovers_only_canonical_shape --lib --locked -- --exact --test-threads=1
cargo test -p orca-runtime --test runtime_surface_commit --locked -- --test-threads=1
```

Expected: PASS.

## Task 3: Adopt and recover during recorded cold startup

**Files:**
- Modify: `crates/orca-runtime/src/session.rs:388-404`
- Modify: `crates/orca-runtime/src/runtime_host.rs:7414-7595, 7850-8005`
- Modify: `crates/orca-runtime/src/runtime_surface/store.rs:3160-3250, 3920-3990`
- Test: `crates/orca-runtime/src/runtime_host.rs:43580-43880`

- [x] **Step 1: Write the restart RED test.**

Replace the active portion of the previous broad exclusion assertion with the
exact test
`recorded_restart_adopts_and_stops_registry_only_running_main_session_once`.
Create a recorded thread, shut down its host, persist one registry-only running
main-session, then resume it.

Assert the first resumed snapshot contains exactly one matching task with:

- `MainSession`, terminal `Stopped`, revision two, original presentation facts,
  parent operation, and historical background fence;
- one parent operation in history with generation zero transferred then
  stopped and terminal `AbortedByRuntimeRestart`; and
- no live background operation after terminalization.

Assert the persistent legacy row is now stopped with the runtime-recovery
summary. Restart again and assert one task, one parent operation, and one
five-event adoption group in the recovered ledger.

- [x] **Step 2: Run RED.**

```bash
cargo test -p orca-runtime runtime_host::tests::recorded_restart_adopts_and_stops_registry_only_running_main_session_once --lib --locked -- --exact --test-threads=1
```

Expected: FAIL because the running row is absent from the surface.

- [x] **Step 3: Implement canonical batch construction and startup ordering.**

Add a focused helper that opens the lock-scoped receipt, subtracts task ids
already present in the recovered snapshot, returns without a commit when no
records remain, and builds repeated five-event groups in receipt order.

For recorded Resume sessions, attach the task registry without running the
blanket interrupted-task rewrite before typed-surface bootstrap. Preserve that
rewrite for new sessions and explicit standalone registry recovery.

Use fresh UUIDv7 operation/request/turn/fence identities, monotonically
increasing reservation sequences, current thread owner/settings/policy, one
canonical fingerprint, `NonReplayable::Missing` with unavailable capsule, and
random opaque background tokens. Preserve only the task facts named by the
Spec. Commit through the dedicated authority with bounded semantic retries.

Call the helper immediately after cold-owner materialization and before the
existing operation-id recovery scan. Do not mutate the registry from the
receipt callback. Let existing `recover_operation` and
`reconcile_terminal_main_session_tasks` perform terminalization and mirroring.

- [x] **Step 4: Run restart GREEN and exclusion regressions.**

Keep `recorded_restart_does_not_reconcile_active_approval_failed_or_rich_tasks`
but change its active negatives to queued, paused, and stopping. Approval,
failed, and rich rows remain unchanged.

```bash
cargo test -p orca-runtime runtime_host::tests::recorded_restart_adopts_and_stops_registry_only_running_main_session_once --lib --locked -- --exact --test-threads=1
cargo test -p orca-runtime runtime_host::tests::recorded_restart_does_not_reconcile_active_approval_failed_or_rich_tasks --lib --locked -- --exact --test-threads=1
cargo test -p orca-runtime runtime_host::tests::recorded_restart_reconciles_registry_only_terminal_main_session_task_once --lib --locked -- --exact --test-threads=1
```

Expected: PASS.

- [x] **Step 5: Write the bounded append-failure RED test.**

Add
`recorded_restart_active_adoption_append_failure_is_bounded_and_non_mutating`.
Add a path-scoped test injector in `JsonlSurfaceCommitLedger` that fails only
active-adoption batches for exactly
`SURFACE_SEMANTIC_COMMIT_RETRY_ATTEMPTS` attempts.

Assert startup returns `ThreadStartFailed`, the legacy row remains running and
unmodified, and a later start without injected failures commits/adopts/stops
the row exactly once.

```bash
cargo test -p orca-runtime runtime_host::tests::recorded_restart_active_adoption_append_failure_is_bounded_and_non_mutating --lib --locked -- --exact --test-threads=1
```

Expected: FAIL before the injector/producer handling, then PASS.

- [x] **Step 6: Run surface storage and recovery regressions.**

```bash
cargo test -p orca-runtime runtime_surface::store::tests --lib --locked -- --test-threads=1
cargo test -p orca-runtime runtime_surface::commit::tests::recovery_ --lib --locked -- --test-threads=1
cargo test -p orca-runtime --test runtime_surface_reducer --locked -- --test-threads=1
```

Expected: PASS with no reducer production change.

## Task 4: Prove projection-only behavior and compatibility

**Files:**
- Test if a focused fixture is required: `crates/orca-tui/src/app.rs`
- Verify: `crates/orca-tui/src/surface_projection.rs`
- Verify: `crates/orca-tui/src/surface_client.rs`

- [x] **Step 1: Run the existing projection and task-control regressions.**

```bash
cargo test -p orca-tui surface_projection --lib --locked -- --test-threads=1
cargo test -p orca-tui task_foreground --lib --locked -- --test-threads=1
cargo test -p orca-tui task_stop --lib --locked -- --test-threads=1
```

The stopped adopted task must be displayable from `SurfaceProjectionState` and
must not gain a live controller. If existing focused coverage cannot exercise
the runtime restart snapshot, add one hosted-app test that resumes the session
and inspects the accepted `SurfaceProjectionSynced`; do not add TUI production
code.

- [x] **Step 2: Prove there is no TUI legacy fact fallback.**

```bash
rg -n 'task_summaries\(|TaskRegistry' crates/orca-tui/src --glob '*.rs'
rg -n 'BackgroundTaskSummary' crates/orca-tui/src --glob '*.rs'
```

Manually classify test imports and handle plumbing. Require zero production
reads that use the registry as a task projection source.

## Task 5: Update contract evidence, Spec status, and roadmap

**Files:**
- Modify: `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.md`
- Modify: `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json`
- Modify: `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.digest.json`
- Modify: `docs/superpowers/specs/2026-08-17-runtime-legacy-active-task-adoption.md`
- Modify: `docs/production-roadmap.md`
- Modify: `scripts/validate-runtime-surface-contract.mjs`
- Test: `scripts/test-validate-runtime-surface-contract.mjs`

- [x] **Step 1: Record the implemented boundary.**

Change this Spec to `Implemented` only after Tasks 1-4 are GREEN. Update the
private contract with the exact active receipt/authority/startup recovery
boundary. Update the roadmap's current convergence paragraph, measured file
counts, completed slice record, and next boundary. Keep approval, failed retry,
other active phases, and rich graphs explicit rather than saying generic task
recovery is complete.

- [x] **Step 2: Update validator evidence without weakening it.**

Inventory `with_active_main_session_adoption` and
`commit_active_task_adoption_batch` with call-shaped, path-specific production
anchors. Update source facts and mutation entrypoints only where the actual
ownership boundary changed. Add negative self-tests proving an import, test
name, or enum occurrence cannot satisfy either production anchor.

Update manifest line references/current payloads, compute the canonical
SHA-256 for the exact manifest bytes, and update the digest artifact. Do not
change expected counts merely to silence unexplained validator failures.

- [x] **Step 3: Run documentation and validator checks.**

```bash
node scripts/test-validate-runtime-surface-contract.mjs
node scripts/validate-runtime-surface-contract.mjs
node scripts/test-validate-windows-platform-boundaries.mjs
node scripts/validate-windows-platform-boundaries.mjs
git diff --check
```

Expected: PASS.

## Task 6: Run full gates and independent review

- [x] **Step 1: Run compiler and focused production gates.**

```bash
cargo check -p orca-runtime --tests --locked
cargo check -p orca-tui --tests --locked
cargo test -p orca-runtime tasks::tests::persistent_active_main_session_adoption_receipt_filters_and_sorts_records --lib --locked -- --exact --test-threads=1
cargo test -p orca-runtime runtime_surface::commit::tests::active_task_receipt_ --lib --locked -- --test-threads=1
cargo test -p orca-runtime runtime_surface::commit::tests::prepared_active_task_adoption_recovers_only_canonical_shape --lib --locked -- --exact --test-threads=1
cargo test -p orca-runtime runtime_host::tests::recorded_restart_adopts_and_stops_registry_only_running_main_session_once --lib --locked -- --exact --test-threads=1
cargo test -p orca-runtime runtime_host::tests::recorded_restart_active_adoption_append_failure_is_bounded_and_non_mutating --lib --locked -- --exact --test-threads=1
cargo test -p orca-runtime runtime_host::tests::recorded_restart_does_not_reconcile_active_approval_failed_or_rich_tasks --lib --locked -- --exact --test-threads=1
cargo test -p orca-runtime --test runtime_surface_commit --locked -- --test-threads=1
cargo test -p orca-runtime --test runtime_surface_reducer --locked -- --test-threads=1
```

- [x] **Step 2: Run full serial runtime/TUI and real PTY gates.**

```bash
cargo test -p orca-runtime --lib --locked -- --test-threads=1
cargo test -p orca-tui --lib --locked -- --test-threads=1
cargo test --test tui_pty_contract --locked -- --test-threads=1
```

The PTY target belongs to the root package; do not use
`-p orca-tui --test tui_pty_contract`.

- [x] **Step 3: Run mechanical gates and ownership searches.**

```bash
node scripts/test-validate-runtime-surface-contract.mjs
node scripts/validate-runtime-surface-contract.mjs
node scripts/test-validate-windows-platform-boundaries.mjs
node scripts/validate-windows-platform-boundaries.mjs
cargo fmt --all -- --check
git diff --check
rg -n 'with_active_main_session_adoption|commit_active_task_adoption_batch' crates/orca-runtime/src --glob '*.rs'
rg -n 'task_summaries\(|TaskRegistry|BackgroundTaskSummary' crates/orca-tui/src --glob '*.rs'
```

Require one receipt owner, one commit entrypoint, one runtime startup producer,
no ordinary actor bypass, and no TUI task-fact fallback.

- [x] **Step 4: Run independent review.**

Review the complete diff against this Spec and base `baeca39b41`. Use
CodeRabbit when available, then manually audit eligibility, outcome-file/task
lock ordering, receipt lifetime, batch shape, fresh and recovered authority,
startup ordering, recovery terminal semantics, repeated-restart identity,
append-failure behavior, compatibility, validator integrity, and test blind
spots. Fix every Critical or Important finding and rerun affected plus full
gates.

## Task 7: Commit, rebase, integrate, verify, and clean up

- [x] **Step 1: Create one semantic topic commit.**

```bash
git status --short
git diff --stat
git add crates/orca-runtime/src/tasks.rs \
  crates/orca-runtime/src/runtime_surface/commit.rs \
  crates/orca-runtime/src/runtime_surface/mod.rs \
  crates/orca-runtime/src/runtime_surface/store.rs \
  crates/orca-runtime/src/runtime_host.rs \
  crates/orca-runtime/src/session.rs \
  docs/production-roadmap.md \
  docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.md \
  docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json \
  docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.digest.json \
  docs/superpowers/specs/2026-08-17-runtime-legacy-active-task-adoption.md \
  docs/superpowers/plans/2026-08-17-runtime-legacy-active-task-adoption.md \
  scripts/validate-runtime-surface-contract.mjs \
  scripts/test-validate-runtime-surface-contract.mjs
git commit -m "feat(runtime): adopt legacy active tasks"
```

Remove absent optional paths from the explicit staging list and add only
compiler-required or validator-required files that were actually reviewed. Do
not stage unrelated work.

- [x] **Step 2: Rebase on current local main and rerun all gates.**

```bash
git rebase main
```

Rerun Task 6 in the rebased worktree. Pre-rebase evidence does not count.

- [x] **Step 3: Fast-forward local main and verify from the root checkout.**

With the root checkout clean:

```bash
git merge --ff-only codex/runtime-legacy-active-task-adoption
```

Rerun compiler checks, exact receipt/authority/restart/failure tests, full
serial runtime/TUI suites, root PTY, validators/self-tests, formatter, and diff
check from local `main`. Do not push or publish.

- [x] **Step 4: Immediately remove the finished worktree and topic branch.**

After root verification succeeds:

```bash
git worktree remove .worktrees/runtime-legacy-active-task-adoption
git branch -d codex/runtime-legacy-active-task-adoption
git worktree list --porcelain
```

Confirm this finished worktree and branch are gone. Preserve every unrelated
worktree and branch. Cleanup is part of completion, not a later follow-up.
