# Runtime Legacy Terminal Task Reconciliation Implementation Plan

> **For Codex:** Execute this plan with RED/GREEN discipline. Do not broaden
> eligibility beyond the completed Spec, do not add a TUI registry fallback,
> and do not integrate until every listed gate and review passes.

**Goal:** Admit pre-surface, registry-only, non-retryable terminal main-session
task history into the durable runtime surface through a locked TaskRegistry
receipt and a dedicated commit authority.

**Architecture:** `TaskRegistry` issues an opaque canonical receipt while
holding the persistent session lock. `RuntimeCommitCoordinator` is the only
consumer that can authorize a fresh `TaskPatch::Reconciled`; ordinary actor
authority is narrowed. Recorded-thread startup appends exact eligible terminal
tasks to the recovered surface snapshot, and the existing TUI snapshot
projection displays them without reading the registry or gaining control
authority.

**Tech Stack:** Rust, serde/serde_json, SHA-256, the existing JSONL surface
ledger/reducer/commit coordinator, Tokio runtime host, ratatui TUI projection,
Node contract validators, and Cargo tests.

---

## Task 1: Add the locked legacy-terminal receipt owner

**Files:**
- Modify: `crates/orca-runtime/src/tasks.rs:93-219, 360-430, 2188-2340, 3034-3090`
- Test: `crates/orca-runtime/src/tasks.rs:3300-3650`

- [x] **Step 1: Write the receipt filtering RED test.**

Add
`tasks::tests::persistent_terminal_reconciliation_receipt_filters_and_sorts_records`.
Create persistent rows covering:

- completed, stopped, and cancelled main-session tasks in reverse id order;
- queued, running, paused, stopping, approval-required, and failed tasks;
- a workflow row;
- a leased or worker-owned row;
- a pending-tool/provider/approval row; and
- a terminal row without a completion timestamp; and
- an id that cannot parse as `SurfaceTaskId`.

Assert that the callback sees only the three safe rows, sorted by id, with the
exact session id, source publication revisions, and a deterministic digest.

- [x] **Step 2: Run RED.**

```bash
cargo test -p orca-runtime tasks::tests::persistent_terminal_reconciliation_receipt_filters_and_sorts_records --lib --locked -- --exact --test-threads=1
```

Expected: FAIL because the receipt API does not exist.

- [x] **Step 3: Implement the opaque receipt and lock-scoped callback.**

In `tasks.rs` add crate-private receipt/record types with private fields and
narrow immutable queries. The record contains only canonical mapping inputs;
it must not expose `TaskControl`, a cancellation token, an owned worker, or a
mutable `TaskRecord`.

Add a persistent-only method shaped like:

```rust
pub(crate) fn with_terminal_main_session_reconciliation<R>(
    &self,
    reconcile: impl FnOnce(&LegacyTerminalTaskReceipt) -> R,
) -> Result<Option<R>, String>
```

Acquire the existing session lock, reload `tasks.json`, filter the exact Spec
eligibility, sort by id, compute a canonical SHA-256 digest, and invoke the
callback before releasing the lock. Return `Ok(None)` for process-local
registries. A read error is an error, not an empty receipt.

The publication horizon is positive and overflow-checked. Old rows with
revision zero remain readable by using a positive reconciliation horizon; do
not rewrite them merely to issue a receipt.

- [x] **Step 4: Run GREEN and persistence regressions.**

```bash
cargo test -p orca-runtime tasks::tests::persistent_terminal_reconciliation_receipt_filters_and_sorts_records --lib --locked -- --exact --test-threads=1
cargo test -p orca-runtime tasks::tests::persistent_registry_recovers_interrupted_subagent_task_by_id --lib --locked -- --exact --test-threads=1
cargo test -p orca-runtime tasks::tests::stale_lease_cannot_replace_takeover_terminal --lib --locked -- --exact --test-threads=1
```

Expected: PASS.

## Task 2: Make reducer reconciliation append-safe

**Files:**
- Modify: `crates/orca-runtime/src/runtime_surface/reducer.rs:7834-8020`
- Test: `crates/orca-runtime/tests/runtime_surface_reducer.rs:8390-8480`

- [x] **Step 1: Write reducer RED tests.**

Add exact tests proving:

- `task_reconciliation_accepts_constrained_terminal_history`;
- `task_reconciliation_cannot_omit_or_change_current_tasks`; and
- `task_reconciliation_rejects_actionable_terminal_history`.

The positive task is revision one, `MainSession`, completed/stopped/cancelled,
foreground-neutral, and has no operation/background/workflow/subagent/
interaction identity. Negative cases cover each forbidden identity and
background ownership.

- [x] **Step 2: Run RED.**

```bash
cargo test -p orca-runtime --test runtime_surface_reducer task_reconciliation_ --locked -- --test-threads=1
```

Expected: the positive terminal creation and omission protection fail.

- [x] **Step 3: Strengthen `apply_task_patch`.**

For `TaskPatch::Reconciled`:

- preserve unique-id and source-horizon checks;
- reject omission of any current task;
- require every current task in the payload to be byte-identical in this
  append-only slice;
- retain revision-one queued/running creation;
- permit revision-one terminal creation only for the constrained historical
  main-session shape; and
- reject failed, approval-required, owned, interactive, workflow-linked, or
  subagent-linked terminal creation.

Do not loosen live `Upserted`, status transitions, or ownership transitions.

- [x] **Step 4: Run GREEN and the complete reducer target.**

```bash
cargo test -p orca-runtime --test runtime_surface_reducer task_reconciliation_ --locked -- --test-threads=1
cargo test -p orca-runtime --test runtime_surface_reducer --locked -- --test-threads=1
```

Expected: PASS.

## Task 3: Add receipt-only commit authority

**Files:**
- Modify: `crates/orca-runtime/src/runtime_surface/commit.rs:619-760, 1170-1370, 4290-4515, 4600-4765`
- Test: `crates/orca-runtime/src/runtime_surface/commit.rs:9800-10850`

- [x] **Step 1: Write authority RED tests.**

Add exact unit tests proving:

- `actor_permit_cannot_commit_task_reconciliation`;
- `terminal_task_receipt_authorizes_exact_append_only_batch`; and
- `terminal_task_receipt_rejects_substitution_omission_and_active_rows`.

The first test calls `commit_actor_batch` with a reducer-valid reconciliation
and expects `StalePublisherPermit`. The positive test obtains a real persistent
TaskRegistry receipt, not a test-fabricated token.

- [x] **Step 2: Run RED.**

```bash
cargo test -p orca-runtime runtime_surface::commit::tests::actor_permit_cannot_commit_task_reconciliation --lib --locked -- --exact --test-threads=1
cargo test -p orca-runtime runtime_surface::commit::tests::terminal_task_receipt_ --lib --locked -- --test-threads=1
```

Expected: FAIL because actor authority is still broad and no receipt authority
exists.

- [x] **Step 3: Implement dedicated fresh and recovered authorities.**

Add a private `BatchCommitAuthority::TaskReconciliation` and a crate-private
coordinator method that accepts the opaque receipt. Fresh authorization must
check session/thread identity, one thread-scoped reconciliation event, exact
receipt-derived additions, byte-identical current tasks, unique ids, and the
source horizon.

Narrow `SurfacePublisherPermit::ActorControl` so any reconciliation event makes
the ordinary single-permit path false.

Add a recovered-prepared authority that recognizes only the reducer-safe
append-only terminal shape against the pre-batch snapshot. It may replay an
already prepared ledger batch but must not be exposed as a fresh commit method
and must not authorize active, failed, approval, omission, or replacement
payloads.

- [x] **Step 4: Run GREEN and the complete commit suites.**

```bash
cargo test -p orca-runtime runtime_surface::commit::tests::actor_permit_cannot_commit_task_reconciliation --lib --locked -- --exact --test-threads=1
cargo test -p orca-runtime runtime_surface::commit::tests::terminal_task_receipt_ --lib --locked -- --test-threads=1
cargo test -p orca-runtime runtime_surface::commit::tests::prepared_terminal_task_reconciliation_recovers_only_safe_shape --lib --locked -- --exact --test-threads=1
cargo test -p orca-runtime --test runtime_surface_commit --locked -- --test-threads=1
```

Expected: PASS.

## Task 4: Reconcile during recorded-thread cold startup

**Files:**
- Modify: `crates/orca-runtime/src/runtime_host.rs:7414-7520, 7790-7935`
- Test: `crates/orca-runtime/src/runtime_host.rs:43380-43520`
- Test: `crates/orca-tui/src/app.rs:1570-1735, 2400-2505`

- [x] **Step 1: Write runtime restart RED tests.**

Add exact runtime tests proving:

- `recorded_restart_reconciles_registry_only_terminal_main_session_task_once`,
  which covers both import and ledger-level idempotence; and
- `recorded_restart_does_not_reconcile_active_approval_failed_or_rich_tasks`.

Seed the persistent registry before starting the recorded runtime. Read the
typed surface snapshot, assert exact revision-one/non-actionable mapping, then
restart again and assert one task plus no extra reconciliation cursor change.

- [x] **Step 2: Run RED.**

```bash
cargo test -p orca-runtime runtime_host::tests::recorded_restart_reconciles_registry_only_terminal_main_session_task_once --lib --locked -- --exact --test-threads=1
cargo test -p orca-runtime runtime_host::tests::recorded_restart_does_not_ --lib --locked -- --test-threads=1
```

Expected: the registry-only terminal task is absent.

- [x] **Step 3: Implement the cold-start producer.**

Add a focused helper that:

1. opens the lock-scoped receipt;
2. subtracts ids already present in the recovered surface snapshot;
3. returns without building a batch when no additions remain;
4. maps additions to constrained `SurfaceTask` values;
5. retains current tasks byte-for-byte and in current snapshot order, then
   appends receipt-sorted additions deterministically;
6. builds one `TaskPatch::Reconciled` batch with an overflow-checked source
   horizon; and
7. commits through the dedicated receipt authority with the existing bounded
   semantic retry classification.

Call it after surface ledger/owner recovery has established the current
snapshot and before the final startup projection/hub binding. Map read or
definitive commit failures to `RuntimeHostError::ThreadStartFailed`. Do not
mutate the registry and do not send TUI events directly.

- [x] **Step 4: Run runtime GREEN.**

```bash
cargo test -p orca-runtime runtime_host::tests::recorded_restart_reconciles_registry_only_terminal_main_session_task_once --lib --locked -- --exact --test-threads=1
cargo test -p orca-runtime runtime_host::tests::recorded_restart_does_not_ --lib --locked -- --test-threads=1
```

Expected: PASS.

- [x] **Step 5: Write and run the TUI restart GREEN test.**

Add
`app::tests::resumed_tui_projects_reconciled_terminal_legacy_task_as_non_actionable`.
Seed one completed legacy main-session row, resume the recorded session through
the hosted controller, and assert the accepted `SurfaceProjectionSynced`
contains exactly one matching terminal row. Attempted foreground/stop must not
fabricate a success projection or notice. Keep the existing approval-only
regression unchanged and passing.

```bash
cargo test -p orca-tui app::tests::resumed_tui_projects_reconciled_terminal_legacy_task_as_non_actionable --lib --locked -- --exact --test-threads=1
cargo test -p orca-tui app::tests::resumed_registry_only_approval_is_not_advertised_as_actionable --lib --locked -- --exact --test-threads=1
```

Expected: PASS after the runtime producer exists.

## Task 5: Update the contract evidence and roadmap

**Files:**
- Modify: `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.md`
- Modify: `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json`
- Modify: `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.digest.json`
- Modify: `docs/production-roadmap.md`
- Modify: `docs/superpowers/specs/2026-08-17-runtime-legacy-terminal-task-reconciliation.md`
- Modify: `docs/superpowers/plans/2026-08-17-runtime-legacy-terminal-task-reconciliation.md`
- Modify if required: `scripts/validate-runtime-surface-contract.mjs`
- Test if required: `scripts/test-validate-runtime-surface-contract.mjs`

- [x] **Step 1: Record the implemented boundary.**

Change this Spec to `Implemented` only after focused GREEN. Update the original
private contract from aspirational receipt language to the exact production
authority and constrained historical terminal import. Update the roadmap with
measured file counts and make the next boundary active/approval/rich-task
reconciliation, not generic cold reconciliation.

- [x] **Step 2: Update validator evidence without weakening it.**

Update path/line anchors, source facts, mutation entrypoints, or closed
inventories only when the source change requires it. Add a path-specific
negative self-test if a new reconciliation entrypoint is inventoried. An import,
enum discriminant, or test occurrence must not satisfy the production anchor.

Regenerate the digest using SHA-256 for the exact reviewed artifacts. Do not
change expected counts merely to silence an unexplained validator failure.

- [x] **Step 3: Run documentation and validator checks.**

```bash
node scripts/test-validate-runtime-surface-contract.mjs
node scripts/validate-runtime-surface-contract.mjs
node scripts/test-validate-windows-platform-boundaries.mjs
node scripts/validate-windows-platform-boundaries.mjs
git diff --check
```

Expected: PASS.

## Task 6: Run full gates and review

- [x] **Step 1: Run compiler and focused production gates.**

```bash
cargo check -p orca-runtime --tests --locked
cargo check -p orca-tui --tests --locked
cargo test -p orca-runtime tasks::tests::persistent_terminal_reconciliation_receipt_filters_and_sorts_records --lib --locked -- --exact --test-threads=1
cargo test -p orca-runtime --test runtime_surface_reducer task_reconciliation_ --locked -- --test-threads=1
cargo test -p orca-runtime --test runtime_surface_commit --locked -- --test-threads=1
cargo test -p orca-runtime runtime_host::tests::recorded_restart_ --lib --locked -- --test-threads=1
cargo test -p orca-tui app::tests::resumed_tui_projects_reconciled_terminal_legacy_task_as_non_actionable --lib --locked -- --exact --test-threads=1
cargo test -p orca-tui app::tests::resumed_registry_only_approval_is_not_advertised_as_actionable --lib --locked -- --exact --test-threads=1
```

- [x] **Step 2: Run full serial runtime/TUI and real PTY gates.**

```bash
cargo test -p orca-runtime --lib --locked -- --test-threads=1
cargo test -p orca-tui --lib --locked -- --test-threads=1
cargo test --test tui_pty_contract --locked -- --test-threads=1
```

The PTY target belongs to the root package; do not use
`-p orca-tui --test tui_pty_contract`.

- [x] **Step 3: Run mechanical gates and obsolete-path searches.**

```bash
node scripts/test-validate-runtime-surface-contract.mjs
node scripts/validate-runtime-surface-contract.mjs
node scripts/test-validate-windows-platform-boundaries.mjs
node scripts/validate-windows-platform-boundaries.mjs
cargo fmt --all -- --check
git diff --check
rg -n 'task_summaries\(|TaskRegistry' crates/orca-tui/src --glob '*.rs'
rg -n 'TaskPatch::Reconciled' crates/orca-runtime/src --glob '*.rs'
```

The TUI search may find test setup or crate-private handle plumbing; manually
classify every match and require zero production task-fact fallback. The
reconciliation search must show exactly the type/store/reducer plus the new
receipt-authorized producer/authority, not an ordinary actor call.

- [x] **Step 4: Run independent review.**

Review the complete diff against the Spec and base commit. Use CodeRabbit when
available, then manually audit receipt construction, lock lifetime, permit
narrowing, recovered-prepared authorization, reducer retention, startup order,
projection behavior, compatibility, and test blind spots. Fix every Critical
or Important finding and rerun affected plus full gates.

## Task 7: Commit, rebase, integrate, verify, and clean up

- [ ] **Step 1: Create one semantic topic commit.**

```bash
git status --short
git diff --stat
git add crates/orca-runtime/src/tasks.rs \
  crates/orca-runtime/src/runtime_surface/commit.rs \
  crates/orca-runtime/src/runtime_surface/reducer.rs \
  crates/orca-runtime/src/runtime_surface/store.rs \
  crates/orca-runtime/src/runtime_host.rs \
  crates/orca-runtime/tests/runtime_surface_reducer.rs \
  crates/orca-tui/src/app.rs \
  docs/production-roadmap.md \
  docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.md \
  docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json \
  docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.digest.json \
  docs/superpowers/specs/2026-08-17-runtime-legacy-terminal-task-reconciliation.md \
  docs/superpowers/plans/2026-08-17-runtime-legacy-terminal-task-reconciliation.md \
  scripts/validate-runtime-surface-contract.mjs \
  scripts/test-validate-runtime-surface-contract.mjs
git commit -m "feat(runtime): reconcile legacy terminal tasks"
```

Adjust the explicit staging list only for compiler-required or validator-
required files that were actually reviewed. Do not stage unrelated work.

- [ ] **Step 2: Rebase on current local main and rerun all gates.**

```bash
git rebase main
```

Rerun Task 6 in the rebased worktree. No stale pre-rebase evidence counts.

- [ ] **Step 3: Fast-forward local main and verify from the root checkout.**

With the root checkout clean:

```bash
git merge --ff-only codex/runtime-legacy-terminal-task-reconciliation
```

Rerun compiler checks, the exact restart tests, full serial runtime/TUI suites,
root PTY, validators/self-tests, formatter, and diff check from local `main`.
Do not push or publish.

- [ ] **Step 4: Immediately remove the finished worktree and topic branch.**

After root verification succeeds:

```bash
git worktree remove .worktrees/runtime-legacy-terminal-task-reconciliation
git branch -d codex/runtime-legacy-terminal-task-reconciliation
git worktree list --porcelain
```

Confirm the finished worktree and branch are gone. Preserve every unrelated
worktree and branch. This cleanup is part of completion, not a later follow-up.
