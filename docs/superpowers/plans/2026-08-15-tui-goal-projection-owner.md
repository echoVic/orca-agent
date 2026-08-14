# TUI Goal Projection Ownership Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make authoritative surface snapshots the only production update for the TUI Goal banner and give Goal value, ordering, and presentation deduplication one private owner.

**Architecture:** `TuiSurfaceProjection` keeps reducing typed Goal patches for runtime consistency and the no-progress lifecycle warning, but it no longer emits a second Goal value event. Each relevant batch appends one `SurfaceProjectionSynced` envelope containing the final reducer snapshot, cursor, and optional Goal presentation directive. `SurfaceGoalProjectionState` accepts snapshots by surface cursor, owns the displayed `ThreadGoal`, and presents one acknowledgement per accepted cursor. Idle mutations re-read and validate an authoritative post-commit snapshot; query results remain display-only.

**Tech Stack:** Rust, `orca-runtime` typed surface reducer, Ratatui TUI state, Cargo behavior tests, Node runtime-surface contract validator.

---

### Task 1: Prove The Competing Goal Fact Paths

**Files:**
- Modify: `crates/orca-tui/src/types.rs:4740-4920`
- Modify: `crates/orca-tui/src/types.rs:5840-5980`
- Modify: `crates/orca-tui/src/app.rs:5289-5400`

- [x] **Step 1: Add a same-usage stale Goal RED test**

Use the existing `SurfaceProjectionState` factory in the projection consistency
tests. Apply a snapshot containing `goal_new`, then a second same-session,
equal-usage-revision snapshot containing `goal_old`. Assert the displayed Goal
remains `goal_new`.

Name the test `surface_goal_projection_rejects_equal_usage_stale_snapshot`.
Before the cursor owner exists it must compile and fail because
`apply_surface_projection_state` assigns the second `current_goal`.

- [x] **Step 2: Add a query-is-presentation-only RED test**

Seed AppState with a projection containing one Goal, deliver
`TuiEvent::GoalStatus` with a different Goal, and assert:

```rust
assert_eq!(state.current_goal.as_ref(), Some(&committed));
assert!(state.messages.iter().any(|message| {
    matches!(message, ChatMessage::System(text) if text.contains(&queried.objective))
}));
```

Name the test `goal_status_is_presentation_only`. It must fail only on the owner
assertion; the query feedback must already be visible.

- [x] **Step 3: Require projection snapshots from restored edit and clear**

In
`preloaded_goal_edit_and_clear_restore_the_runtime_surface_before_mutation`,
replace success waits for `GoalUpdated`/`GoalCleared` with waits for
`SurfaceProjectionSynced` whose `current_goal` is respectively the edited Goal
and `None`. Fail immediately if a granular Goal event arrives before the
expected snapshot.

- [x] **Step 4: Require an authoritative pause projection**

In `active_goal_pause_bypasses_command_backlog_and_cancels_goal_run`, wait for a
`SurfaceProjectionSynced` payload whose Goal is paused. Reject a preceding
`GoalUpdated`. Keep the existing elapsed-time bound and persisted-state
assertions so the test still proves the bypass/cancellation behavior.

- [x] **Step 5: Run the four tests and record intended RED**

```bash
cargo test -p orca-tui surface_goal_projection --lib --locked -- --test-threads=1
cargo test -p orca-tui goal_status_is_presentation_only --lib --locked -- --test-threads=1
cargo test -p orca-tui preloaded_goal_edit_and_clear_restore_the_runtime_surface_before_mutation --lib --locked -- --test-threads=1
cargo test -p orca-tui active_goal_pause_bypasses_command_backlog_and_cancels_goal_run --lib --locked -- --test-threads=1
```

Expected: the owner tests fail on direct replacement, and the real-action tests
fail because idle mutations send granular Goal events instead of a snapshot.
Compilation/setup errors are not accepted RED evidence.

### Task 2: Add The Cursor-Fenced Goal Owner And Atomic Presentation Envelope

**Files:**
- Modify: `crates/orca-tui/src/surface_projection.rs:25-120`
- Modify: `crates/orca-tui/src/surface_projection.rs:260-286`
- Modify: `crates/orca-tui/src/types.rs:850-1060`
- Modify: `crates/orca-tui/src/types.rs:1290-1425`
- Modify: `crates/orca-tui/src/types.rs:2368-2415`

- [x] **Step 1: Extend the projection envelope with cursor and presentation**

Add a crate-private presentation enum:

```rust
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GoalProjectionPresentation {
    Updated,
    Cleared,
}
```

Extend `SurfaceProjectionState` with `cursor: SurfaceCursor` and
`goal_presentation: Option<GoalProjectionPresentation>`.
`from_surface_snapshot` clones `snapshot.cursor` and defaults presentation to
`None`. Add a consuming `with_goal_presentation` helper for typed and idle
mutation paths. Do not expose public setters.

- [x] **Step 2: Implement `SurfaceGoalProjectionState`**

Place the private owner beside `SurfaceProjectionState`:

```rust
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SurfaceGoalProjectionState {
    current: Option<ThreadGoal>,
    cursor: Option<SurfaceCursor>,
    presented_cursor: Option<SurfaceCursor>,
}
```

Its projection transition must:

- accept the first snapshot;
- reject a different thread/incarnation until reset;
- reject lower `next_seq`;
- accept higher `next_seq` and replace the Goal;
- treat equal cursor/equal Goal as idempotent;
- reject equal cursor/different Goal and preserve the owner;
- process `Updated` only with a current Goal and `Cleared` only without one;
- return a presentation effect once per exact accepted cursor.

Add immutable `current()`, `reset()`, and debug consistency methods. A test-only
replacement helper may set the current Goal for renderer fixtures, but it must
be `#[cfg(test)]` and unavailable to production callers.

- [x] **Step 3: Replace the public mutable AppState field**

Replace `pub current_goal: Option<ThreadGoal>` with one
`pub(crate) surface_goal: SurfaceGoalProjectionState`, initialize it in
`Default`, and reset it in `reset_session_projection`.

Add the public immutable migration query in `surface_projection.rs`:

```rust
impl AppState {
    pub fn current_goal(&self) -> Option<&ThreadGoal> {
        self.surface_goal.current()
    }
}
```

- [x] **Step 4: Apply snapshot and presentation atomically**

In `apply_surface_projection_state`, reset the Goal owner when the session
changes, apply the projection once, then handle the returned effect:

- `Updated(goal)` uses `format_goal_notice`, preserves Running for a continuing
  Goal, and otherwise becomes Idle exactly as the old branch did;
- `Cleared` finishes the assistant stream, appends `Goal cleared.`, and becomes
  Idle.

Keep history hydration silent because its envelope has no directive. Change
`GoalStatus` to format feedback/status without assigning the owner. Delete the
old Goal mutation arms only after Task 4 migrates all senders.

- [x] **Step 5: Enrich the RED owner tests with real cursors and reach GREEN**

Update all `SurfaceProjectionState` test factories with deterministic cursors.
Use sequence 2 for the committed Goal and sequence 1 for the stale Goal. Add
equal replay, contradictory equal-cursor, different-incarnation-before-reset,
session-reset, and once-per-cursor presentation cases.

Run the two Task 1 owner commands. Expected: both pass, with query feedback
visible and the committed Goal unchanged.

### Task 3: Make Typed Goal Commits Snapshot-Only

**Files:**
- Modify: `crates/orca-tui/src/surface_projection.rs:780-925`
- Modify: `crates/orca-tui/src/surface_projection.rs:1497-end`

- [x] **Step 1: Remove granular Goal value publication from reduction**

Keep `TuiSurfaceProjection.goal` and the `SurfaceGoal` patch application needed
for reducer consistency and no-progress transition detection. Remove every
`GoalUpdated` and `GoalCleared` push. Removed receipts set the internal Goal to
`None` and continue without publishing a partial TUI fact.

Keep the no-progress `Notice`; it is a lifecycle warning, not a Goal fact.

- [x] **Step 2: Annotate the final committed snapshot**

In `project_typed_batch`, detect whether the accepted batch contains any
`SurfaceEvent::Goal`. After reduction, build `SurfaceProjectionState` from the
final reducer snapshot and attach `Updated` when its Goal is present or
`Cleared` when absent. Append that one envelope after any lifecycle events.

Usage, context, task, workflow, operation, subagent, and session behavior stays
unchanged.

- [x] **Step 3: Add typed projection ordering coverage**

Use an existing live Goal action fixture or add a valid Goal commit fixture to
assert that the projected event sequence contains no granular Goal fact and
ends in one `SurfaceProjectionSynced` whose cursor equals the batch
`cursor_after`, whose Goal equals the reducer snapshot, and whose presentation
matches the final presence/absence.

Name creation/update and removal coverage with the `typed_goal_projection`
prefix so the focused command is stable.

- [x] **Step 4: Run typed projection tests**

```bash
cargo test -p orca-tui typed_goal_projection --lib --locked -- --test-threads=1
```

Expected: creation/update and removal both pass, and a missing reducer snapshot
still returns `SurfaceProjectionError::MissingReducerSnapshot` without a Goal
mutation.

### Task 4: Project Idle Goal Mutations From Authoritative Snapshots

**Files:**
- Modify: `crates/orca-tui/src/surface_client.rs:1040-1140`
- Modify: `crates/orca-tui/src/surface_actions.rs:204-226`
- Modify: `crates/orca-tui/src/app.rs:8588-8714`
- Modify: `crates/orca-tui/src/app.rs:9457-9550`

- [x] **Step 1: Add a post-commit projection helper**

After an idle Goal mutation commits, detach the mutation attachment and call the
existing fresh `read_snapshot`. Validate that the snapshot cursor has the same
thread id and incarnation as `GoalMutationOutput.change_cursor`, and that its
`next_seq` is greater than or equal to the committed cursor. Return an error
containing `Goal mutation committed but TUI projection failed` when attachment,
identity, or ordering proof fails.

Convert the snapshot with `SurfaceProjectionState::from_surface_snapshot` and
derive `Updated` or `Cleared` from that final snapshot. If a later concurrent
Goal commit is already visible, the acknowledgement describes that newer fact
instead of the initiating command's stale intent. The mutation output's
`ThreadGoal` is not sent to AppState.

- [x] **Step 2: Return projection envelopes from edit, clear, and pause**

Change the crate-private `surface_client` and `TuiSurfaceActions` methods for
edit, clear, and pause to return `SurfaceProjectionState`. Existing no-Goal,
stale-fence, deferred, and uncommitted errors remain errors. Do not create a
background reader or retry loop.

- [x] **Step 3: Send only the projection from app command branches**

On successful edit, clear, or pause, send
`TuiEvent::SurfaceProjectionSynced(Box::new(projection))`. Remove the `Ok(None)`
edit branch because every successful mutation returns a projection. If a later
concurrent commit removes the edited Goal before the fresh read, that final
authoritative snapshot carries `Cleared` rather than fabricating edit success.

In `resume_latest_active_goal_hosted`, retain the visible restored-session
`Notice` but remove the early `GoalUpdated`; the committed typed Goal batch owns
the banner update.

- [x] **Step 4: Run restored mutation and pause tests to GREEN**

Run the two Task 1 real-action commands. Expected: edit, clear, and pause are
observed as authoritative snapshots, existing persistence/cancellation checks
still pass, and no granular Goal event arrives.

### Task 5: Delete Old Events And Migrate All Readers And Tests

**Files:**
- Modify: `crates/orca-tui/src/types.rs`
- Modify: `crates/orca-tui/src/app.rs`
- Modify: `crates/orca-tui/src/slash_command_actions.rs`
- Modify: `crates/orca-tui/src/ui.rs`
- Modify: `crates/orca-tui/src/channels.rs`
- Modify if compiler evidence requires: other `crates/orca-tui/src/*.rs`

- [x] **Step 1: Remove the obsolete TuiEvent variants**

Delete `GoalUpdated(ThreadGoal)` and `GoalCleared` from `TuiEvent` and their
AppState reducer arms. Keep `GoalStatus(Option<ThreadGoal>)` as display-only
query feedback. Update generic channel-capacity tests to use a fact-neutral
event such as `Tick` or `Notice`.

- [x] **Step 2: Migrate production reads to `current_goal()`**

Update footer/banner rendering and `/status` code in `ui.rs` and
`slash_command_actions.rs`. Bind the borrowed Goal once per rendering function.
No production code may call the test-only replacement helper.

- [x] **Step 3: Migrate behavior tests from payload events to snapshots**

For tests that need to inspect committed Goal state, match
`SurfaceProjectionSynced` and inspect its `current_goal`/cursor. For tests that
only verify user feedback, apply the event to AppState and assert the rendered
system message or banner through `current_goal()`.

Use the test-only owner replacement only for isolated UI geometry/formatting
fixtures that have no surface snapshot behavior under test.

- [x] **Step 4: Run all Goal-focused behavior tests**

```bash
cargo test -p orca-tui goal_ --lib --locked -- --test-threads=1
cargo test -p orca-tui current_goal --lib --locked -- --test-threads=1
cargo test -p orca-tui surface_projection_consistency --lib --locked -- --test-threads=1
```

Expected: all focused suites pass with the same Goal command, banner, notice,
pause, resume, continuation, and restored-session behavior.

- [x] **Step 5: Verify the ownership/deletion boundary**

```bash
rg -n 'GoalUpdated|GoalCleared' crates/orca-tui/src
rg -n '\.current_goal\b|current_goal\s*=' crates/orca-tui/src --glob '*.rs'
rg -n 'surface_goal' crates/orca-tui/src --glob '*.rs'
```

Expected: the first search is empty; the second shows snapshot payload fields,
immutable method calls, and test construction only, with no AppState field or
production assignment; the final search shows one AppState owner and its
implementation/tests.

### Task 6: Refresh Architecture Evidence And Validator Anchors

**Files:**
- Modify: `docs/production-roadmap.md`
- Modify: `docs/superpowers/specs/2026-08-15-tui-goal-projection-owner.md`
- Modify: `docs/superpowers/plans/2026-08-15-tui-goal-projection-owner.md`
- Modify if validation reports moved anchors: `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json`
- Modify if reviewed artifacts change: `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.digest.json`

- [x] **Step 1: Update the roadmap with the completed Goal slice**

Record the private Goal owner, cursor fencing, snapshot-only fact updates,
presentation-only queries, idle post-commit snapshot proof, and deletion of the
granular events/public mutable field. Keep session identity, workflow, and
operation projection explicitly open. Do not claim release completion.

- [x] **Step 2: Run the runtime-surface validator and repair only factual drift**

```bash
node scripts/validate-runtime-surface-contract.mjs
```

Update only reported current-source anchors or reviewed digests. Do not weaken
closed inventories, change runtime Goal facts, or add a harmless baseline.

- [x] **Step 3: Recompute reviewed artifact digests structurally if needed**

Use `shasum -a 256` for the exact reviewed artifacts named in the digest JSON,
update only changed SHA-256 values, and rerun the validator.

- [x] **Step 4: Mark Spec and plan implemented only after fresh gates**

Update status/self-review and task checkboxes with exact RED/GREEN evidence,
test counts, and review disposition. Record the operational rebase, integration
commit, and cleanup in the final handoff after Task 8 instead of pre-claiming
work that must happen after this document's semantic commit. The broad goal and
release remain active/pending.

RED evidence: the original owner tests allowed an equal-usage stale snapshot
and `GoalStatus` to replace the committed Goal; the restored edit/clear and
active pause tests observed granular Goal events before an authoritative
snapshot. GREEN evidence: typed Goal projection 3/3, Goal owner projection 3/3,
presentation-only Goal status 1/1, restored edit/clear 1/1, and active pause
1/1. Fresh full gates passed with 1,050 serial TUI library tests, 6 root PTY
contract tests, both Node validator self-tests, the direct runtime-surface
validator, locked TUI check, formatting, and diff integrity.

### Task 7: Run Full Verification And Independent Review

**Files:**
- Review all changed files from `git diff --stat` and `git diff`

- [x] **Step 1: Run locked compile and full TUI behavior gates**

```bash
cargo check -p orca-tui --tests --locked
cargo test -p orca-tui --lib --locked -- --test-threads=1
cargo test --test tui_pty_contract --locked -- --test-threads=1
```

All must exit 0. Report exact counts. This projection-only slice does not alter
provider lowering or DeepSeek API behavior, so credentialed real-API smoke is
not required; the real runtime surface action fixtures and root PTY boundary
are the relevant behavioral gates.

- [x] **Step 2: Run validator and hygiene gates**

```bash
node --test scripts/test-validate-runtime-surface-contract.mjs
node --test scripts/test-validate-windows-platform-boundaries.mjs
node scripts/validate-runtime-surface-contract.mjs
cargo fmt --all -- --check
git diff --check
```

- [x] **Step 3: Audit every acceptance item and the full diff**

Inspect `git diff --stat`, `git diff`, all Goal senders/readers, owner cursor
comparisons, post-commit error wording, public migration docs, reset/restart
coverage, and stale/duplicate presentation behavior. Confirm no unrelated
worktree file changed.

- [x] **Step 4: Request independent review**

Use `superpowers:requesting-code-review` with the full branch diff and Spec.
Require findings ordered by severity with file/line evidence and explicit
checks for stale cursor handling, duplicate presentation, mutation commit vs
projection failure, query ownership, cancellation/pause behavior, compatibility,
and missing tests. Resolve every Critical and Important finding, rerun affected
focused tests, and repeat review until none remain.

Independent review initially found one Important coverage mismatch and three
Minor coverage/documentation gaps. Direct reducer-backed creation, edit,
removal, and missing-reducer tests; a higher non-Goal cursor fence; silent
startup hydration; and corrected final-winner wording resolved them. Fresh
re-review reported no remaining Critical, Important, or Minor findings.

### Task 8: Commit, Rebase, Integrate, And Clean Up

**Files:**
- Stage only files named by `git diff --name-only`

- [ ] **Step 1: Create one semantic feature commit**

After all review findings and fresh gates are green, stage the complete slice
and commit:

```bash
git commit -m "refactor(tui): own goal surface projection"
```

Do not split tests/docs from the owner and do not include unrelated changes.

- [ ] **Step 2: Rebase latest local main and reverify**

Fetch `origin main`, verify the main checkout is clean, and rebase the feature
branch onto current local `main`. Resolve conflicts by preserving both latest
main behavior and the Spec. Rerun focused Goal tests, full serial TUI, root PTY,
validators, formatting, and diff checks on the rebased branch.

- [ ] **Step 3: Fast-forward clean local main and verify integrated state**

From the main checkout, require clean status and fast-forward `main` to the
reviewed feature commit. Rerun full TUI, root PTY, validators, formatting, and
status checks on integrated main. Do not push, tag, release, or publish this
architecture-only slice.

- [ ] **Step 4: Remove only the owned worktree and branch**

After confirming `.worktrees/tui-goal-projection-owner` is clean and its HEAD
equals integrated main, remove that worktree and delete
`codex/tui-goal-projection-owner`. Preserve every unrelated worktree, branch,
and untracked user file.

## Plan Self-Review

- Every Spec behavior maps to a task: competing-source RED evidence (Task 1),
  cursor/owner/presentation state (Task 2), typed commits (Task 3), idle mutation
  proof (Task 4), old-path deletion and caller migration (Task 5), architecture
  evidence (Task 6), full verification/review (Task 7), and linear integration
  plus cleanup (Task 8).
- No task introduces a second Goal cache, local mutation fallback, background
  reader, unbounded wait, protocol change, or durable format.
- Normal, cancellation, rejection, timeout, retry, disconnect, reset, restart,
  duplicate, and stale delivery behaviors all have an owner rule or behavior
  gate.
- Public Rust source breaks and their immutable migration path are explicit;
  CLI, TUI workflow, server/JSONL, ACP, and persistence compatibility remain
  intact.
- There are no unresolved markers or unspecified deletion conditions. Session,
  workflow, and operation projection are explicit later boundaries rather than
  unfinished Goal work.
