# TUI Foreground And Recoverable Operation Projection Ownership Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the authoritative surface snapshot the only TUI operation-fact
update, with one private owner for foreground and recoverable operation ids.

**Architecture:** `SurfaceProjectionState` derives both ids from one
`SurfaceSnapshot`. `SurfaceSessionProjectionState` validates the envelope,
then `SurfaceOperationProjectionState` atomically replaces the two local ids
and returns a presentation-only recovery effect. Startup sends only the
projection envelope, and commands borrow immutable ids from `AppState`.

**Tech Stack:** Rust, `orca-runtime` typed surface snapshots, Ratatui TUI
state, Cargo behavior tests, Node runtime-surface contract validator.

### Task 1: Prove The Competing Recovery Fact Paths

**Files:**
- Modify: `crates/orca-tui/src/surface_projection.rs`
- Modify: `crates/orca-tui/src/types.rs`
- Modify: `crates/orca-tui/src/app.rs`

- [ ] **Step 1: Add a snapshot recovery RED test**

Construct a suspended replayable user-operation snapshot through the existing
runtime surface test helpers. Assert `SurfaceProjectionState::from_surface_snapshot`
contains its operation id as both foreground and recoverable. Before envelope
completion, the recoverable assertion must fail for the intended behavior.

- [x] **Step 2: Add operation owner ordering RED tests**

Apply an accepted recovery projection, then assert that a `HistoryLoaded`,
`TurnStarted`, and `SessionCompleted` sequence cannot clear the id. Apply the
next accepted no-recovery snapshot and assert that it clears the owner and
prompt. Add equal replay, changed recovery id, rejected stale cursor,
cross-incarnation-before-reset, and reset-to-new-session cases.

- [x] **Step 3: Require startup to be envelope-only**

Extend the existing runtime-ready harness test to inspect its ordered events.
It must receive one `SurfaceProjectionSynced` carrying recovery and must not
receive `RecoveryAvailable`. Before the migration, this fails because startup
sends the granular event.

- [x] **Step 4: Run intended RED evidence**

```bash
cargo test -p orca-tui surface_operation_projection --lib --locked -- --test-threads=1
cargo test -p orca-tui runtime_ready --lib --locked -- --test-threads=1
```

The first RED test was run as
`cargo test -p orca-tui recovery_projection_is_not_overwritten_by_lifecycle_events --lib --locked -- --test-threads=1`.
It failed because `TurnStarted` cleared the accepted recoverable id. The
producer test is now `runtime_ready_emits_only_attachment_and_snapshot_projection`.
The dedicated positive projection test from a suspended runtime fixture remains
an intentional coverage follow-up; the runtime recovery predicate itself is
covered by `orca-runtime` cold-recovery tests.

### Task 2: Complete The Snapshot Envelope And Add The Private Owner

**Files:**
- Modify: `crates/orca-tui/src/surface_projection.rs`
- Modify: `crates/orca-tui/src/types.rs`

- [x] **Step 1: Add recoverability to `SurfaceProjectionState`**

Add crate-private `recoverable_operation_id: Option<SurfaceOperationId>`.
`from_surface_snapshot` must call
`snapshot.recoverable_user_operation().map(|operation| operation.operation_id().clone())`.
Update every explicit test projection literal with `None` unless it models a
recoverable operation.

- [x] **Step 2: Implement `SurfaceOperationProjectionState`**

Place the owner beside the other projection owners:

```rust
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SurfaceOperationProjectionState {
    foreground_operation_id: Option<SurfaceOperationId>,
    recoverable_operation_id: Option<SurfaceOperationId>,
}
```

Its `apply_projection` replaces both ids only after session-envelope admission,
returns an effect for a new/different recoverable id, and returns a clear effect
when recovery disappears. It tracks the accepted surface cursor only to reject
lower, cross-identity, or equal-cursor-contradictory operation envelopes. It
exposes immutable borrow queries and `reset()`; it has no worker. A recoverable
id must equal the foreground id. Add unit coverage for every Task 1 ordering
case.

- [x] **Step 3: Replace mutable AppState fact fields**

Replace `active_surface_operation_id` and public
`recoverable_operation_id` with one `surface_operation` owner. Initialize and
reset it with the other session projection owners. Add public immutable
`foreground_operation_id()` and `recoverable_operation_id()` queries. Update
the projection consistency assertion to compare owner values.

- [x] **Step 4: Apply facts and prompt presentation atomically**

In `apply_surface_projection_state`, apply the operation owner after session
admission and handle its effect: a newly recoverable operation shows the
existing prompt/notice, while `None` hides the prompt and resets its selection.
History hydration has no presentation directive. Do not store a duplicate
recovery fact in the prompt state.

- [x] **Step 5: Reach GREEN for Task 1 owner tests**

Run the Task 1 commands. Equal replay must not repeat the notice, rejected
projection must preserve all state, and reset must leave no predecessor id.

### Task 3: Delete The Granular Event And Migrate Producers And Readers

**Files:**
- Modify: `crates/orca-tui/src/app.rs`
- Modify: `crates/orca-tui/src/types.rs`
- Modify: `crates/orca-tui/src/status_key_actions.rs`
- Modify: `crates/orca-tui/src/slash_command_actions.rs`
- Modify if compiler evidence requires: `crates/orca-tui/src/*.rs`

- [x] **Step 1: Make runtime-ready projection-only**

`announce_runtime_ready` sends `MentionRuntimeReady` and the one snapshot
projection. Remove its recovery event and direct notice. The owner effect
preserves user-visible wording exactly once after accepted recovery hydration.

- [x] **Step 2: Delete `RecoveryAvailable`**

Remove the `TuiEvent` variant and AppState reducer arm. Remove all
`HistoryLoaded`, `TurnStarted`, and `SessionCompleted` writes to operation
facts; preserve their status/transcript behavior. Migrate channel and reducer
tests to envelope assertions.

- [x] **Step 3: Migrate command and UI reads**

Use `state.recoverable_operation_id()` in status-key resume handling and slash
cancel/status output. Migrate test setup to apply an authoritative projection,
or use a tightly scoped test-only owner fixture when only renderer geometry is
under test. No production caller may write either id.

- [x] **Step 4: Prove restored action behavior**

Fresh-AppState command tests now hydrate an accepted recovery projection, assert
the immutable operation query through the dispatched `/cancel-operation` action,
and prove the action carries that snapshot id rather than a granular payload.

- [x] **Step 5: Run focused behavior gates**

```bash
cargo test -p orca-tui recoverable_operation --lib --locked -- --test-threads=1
cargo test -p orca-tui recovery_prompt --lib --locked -- --test-threads=1
cargo test -p orca-tui cancel_operation --lib --locked -- --test-threads=1
cargo test -p orca-tui surface_projection_consistency --lib --locked -- --test-threads=1
```

### Task 4: Verify Deletion And Refresh Architecture Evidence

**Files:**
- Modify: `docs/production-roadmap.md`
- Modify: this spec and plan
- Modify if validator reports factual anchor drift:
  `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json`
- Modify if reviewed files change:
  `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.digest.json`

- [x] **Step 1: Verify the ownership boundary**

```bash
rg -n 'RecoveryAvailable' crates/orca-tui/src
rg -n 'recoverable_operation_id\\s*=' crates/orca-tui/src --glob '*.rs'
rg -n 'active_surface_operation_id' crates/orca-tui/src --glob '*.rs'
```

Expected: the first and final searches are empty; the middle search contains
only private-owner initialization or replacement, never AppState direct fact
writes.

- [x] **Step 2: Update the roadmap after behavior is green**

Record the snapshot-only operation owner and event deletion, and keep workflow
task projection explicitly open because runtime task reconciliation remains
unimplemented. Do not claim release completion.

- [x] **Step 3: Run and repair the validator only for factual drift**

```bash
node scripts/validate-runtime-surface-contract.mjs
```

Update anchors or reviewed SHA-256 values only when the validator proves they
changed. Do not weaken inventories or add a compatibility baseline.

### Task 5: Full Verification And Independent Review

- [x] **Step 1: Run locked compile and full behavioral gates**

```bash
cargo check -p orca-tui --tests --locked
cargo test -p orca-tui --lib --locked -- --test-threads=1
cargo test --test tui_pty_contract --locked -- --test-threads=1
```

- [x] **Step 2: Run validator and hygiene gates**

```bash
node --test scripts/test-validate-runtime-surface-contract.mjs
node --test scripts/test-validate-windows-platform-boundaries.mjs
node scripts/validate-runtime-surface-contract.mjs
cargo fmt --all -- --check
git diff --check
```

- [x] **Step 3: Independently review the full diff**

Review stale/reset ordering, snapshot derivation, duplicate prompt effects,
startup/restart behavior, cancellation target reads, rejected projections,
source compatibility, and missing tests. Resolve every Critical or Important
finding, rerun affected tests, and repeat review until none remain. CodeRabbit
reviewed the uncommitted source and roadmap diff against `c073b3a3b` and raised
zero issues.

### Task 6: Commit, Rebase, Integrate, And Clean Up

- [x] **Step 1: Create one semantic feature commit**

Stage only the reviewed files and commit:

```bash
git commit -m "refactor(tui): own operation surface projection"
```

Completed before the required rebase and integration gates.

- [x] **Step 2: Rebase current local main and reverify**

Fetch `origin main`, verify the main checkout is clean, rebase the feature
branch onto current local `main`, and rerun all focused and full Task 5 gates.

- [x] **Step 3: Fast-forward clean local main and verify integrated state**

Fast-forward local `main` only after clean status and successful rebase gates.
Repeat the full TUI, PTY, validator, formatting, diff, and status checks on
integrated `main`. Do not push, tag, release, or publish.

- [x] **Step 4: Remove only this worktree and branch**

After its HEAD equals integrated main and it is clean, remove
`.worktrees/tui-operation-projection-owner` and delete
`codex/tui-operation-projection-owner`. Preserve all unrelated worktrees and
branches.

## Plan Self-Review

The plan maps every specification boundary to a test-first implementation task:
competing facts (Task 1), owner/envelope (Task 2), event and caller deletion
(Task 3), evidence (Task 4), verification/review (Task 5), and linear
integration (Task 6). It does not hide the unresolved workflow registry gap,
introduce a cache, create a worker, or change a durable/external protocol.
