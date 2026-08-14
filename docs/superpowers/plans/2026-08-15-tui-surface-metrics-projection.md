# TUI Surface Metrics Projection Ownership Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the typed reducer snapshot the only production usage/context update boundary and give those local TUI facts one aggregate owner.

**Architecture:** `TuiSurfaceProjection` continues reducing typed runtime commits and emitting visible lifecycle events, but usage and idle-context patches no longer create granular TUI metric events. Each relevant commit ends in `SurfaceProjectionSynced`; `SurfaceMetricsState` in `surface_projection.rs` applies its usage/context revisions and exposes immutable AppState queries. Session, Goal, workflow, and operation projection remain on their existing path for later slices.

**Tech Stack:** Rust, `orca-runtime` typed surface reducer, Ratatui TUI state, Cargo tests, Node runtime-surface contract validator.

---

### Task 1: Prove Granular Metric Publication Is The Duplicate Path

**Files:**
- Modify: `crates/orca-tui/src/surface_projection.rs`

- [x] **Step 1: Change the usage projection test to require commit-boundary publication**

Rename `typed_usage_projection_preserves_usage_revision` to
`typed_usage_projection_waits_for_commit_snapshot`. Keep its existing typed
usage event and batch construction, but replace the granular-event match with:

```rust
let mut projection = TuiSurfaceProjection::from_snapshot(before.clone(), &[]);

assert!(projection
    .reduce_typed_batch(&batch)
    .expect("valid usage batch")
    .is_empty());
```

Keep the existing missing-reducer-snapshot assertion after constructing a fresh
projection. It proves a caller cannot receive a partial metric event when the
commit snapshot is unavailable.

- [x] **Step 2: Add an idle-context RED test**

Use the existing `cursor`, `uuid_v7_bytes`, and `commit_batch_with_events`
helpers to add:

```rust
#[test]
fn typed_context_projection_waits_for_commit_snapshot() {
    let before = cursor(0, 1);
    let commit_class = CommitClass::Recorded {
        thread_owner_epoch: ThreadOwnerEpoch::new(1),
        durable_revision: DurableRevision::try_new(2).unwrap(),
        commit_id: SurfaceCommitId::try_from_bytes(uuid_v7_bytes(18)).unwrap(),
    };
    let event = SurfaceEventEnvelope {
        ordinal: 0,
        event_id: SurfaceEventId::try_from_bytes(uuid_v7_bytes(19)).unwrap(),
        commit_class,
        scope: SurfaceScope::Thread,
        event: SurfaceEvent::Context(orca_runtime::surface::SurfaceContextSnapshot {
            revision: orca_runtime::surface::ContextRevision::try_new(2).unwrap(),
            used_tokens: 4_096,
            limit_tokens: 128_000,
            compaction: orca_runtime::surface::CompactionState::Idle,
            fragments: Vec::new(),
            provider_replay: orca_runtime::surface::ProviderReplayHealth::None,
        }),
    };
    let after = SurfaceCursor {
        next_seq: SequenceNumber::new(1),
        source_revision: CursorSourceRevision::Recorded {
            durable_revision: DurableRevision::try_new(2).unwrap(),
        },
        ..before.clone()
    };
    let batch = commit_batch_with_events(before.clone(), after, vec![event], 20);
    let mut projection = TuiSurfaceProjection::from_snapshot(before, &[]);

    assert!(projection
        .reduce_typed_batch(&batch)
        .expect("valid context batch")
        .is_empty());
}
```

- [x] **Step 3: Run both tests and observe the intended RED failures**

Run:

```bash
cargo test -p orca-tui typed_usage_projection --lib --locked -- --test-threads=1
cargo test -p orca-tui typed_context_projection --lib --locked -- --test-threads=1
```

Expected: each assertion fails because the current reducer returns one granular
`UsageUpdated` or `ContextUpdated` event. Compilation and batch reduction must
succeed; a setup error is not an accepted RED result.

### Task 2: Add One Metrics Owner And Delete The Granular Event Paths

**Files:**
- Modify: `crates/orca-tui/src/surface_projection.rs`
- Modify: `crates/orca-tui/src/types.rs`

- [x] **Step 1: Move the commit snapshot payload beside the projection reducer**

Move `SurfaceProjectionState` from `types.rs` to `surface_projection.rs` without
changing its fields. Import `ThreadGoal` there and keep
`SurfaceProjectionState::from_surface_snapshot` beside the type. Update
`types.rs` to keep a doc-hidden public re-export of the payload rather than
define it, preserving its existing Rust path.

```rust
#[derive(Clone, Debug, PartialEq)]
#[doc(hidden)]
pub struct SurfaceProjectionState {
    pub(crate) session_id: String,
    pub(crate) title: String,
    pub(crate) usage_revision: u64,
    pub(crate) usage: UsageTotals,
    pub(crate) context_revision: u64,
    pub(crate) context_used_tokens: usize,
    pub(crate) context_limit_tokens: usize,
    pub(crate) workflow_tasks: Vec<BackgroundTaskSummary>,
    pub(crate) current_goal: Option<ThreadGoal>,
    pub(crate) foreground_operation_id: Option<SurfaceOperationId>,
}
```

- [x] **Step 2: Define the private aggregate and immutable AppState queries**

Add `SurfaceMetricsState` in `surface_projection.rs`:

```rust
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct SurfaceMetricsState {
    usage: UsageTotals,
    usage_revision: Option<u64>,
    context_revision: Option<u64>,
    context_used_tokens: usize,
    context_limit_tokens: usize,
}

impl SurfaceMetricsState {
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn rejects_usage_revision(&self, revision: u64) -> bool {
        self.usage_revision
            .is_some_and(|current| revision < current)
    }

    pub(crate) fn apply_projection(&mut self, projection: &SurfaceProjectionState) {
        self.usage = projection.usage.clone();
        self.usage_revision = Some(projection.usage_revision);
        if self
            .context_revision
            .is_none_or(|current| projection.context_revision > current)
        {
            self.context_revision = Some(projection.context_revision);
            self.context_used_tokens = projection.context_used_tokens;
            self.context_limit_tokens = projection.context_limit_tokens;
        }
    }

    pub(crate) fn usage(&self) -> &UsageTotals {
        &self.usage
    }

    pub(crate) fn context_used_tokens(&self) -> usize {
        self.context_used_tokens
    }

    pub(crate) fn context_limit_tokens(&self) -> usize {
        self.context_limit_tokens
    }

    pub(crate) fn assert_matches_projection(&self, projection: &SurfaceProjectionState) {
        #[cfg(any(test, debug_assertions))]
        {
            debug_assert_eq!(self.usage, projection.usage);
            debug_assert_eq!(self.usage_revision, Some(projection.usage_revision));
            debug_assert_eq!(self.context_revision, Some(projection.context_revision));
            debug_assert_eq!(self.context_used_tokens, projection.context_used_tokens);
            debug_assert_eq!(self.context_limit_tokens, projection.context_limit_tokens);
        }
    }
}
```

Add AppState read methods in the same module which delegate to the owner:

```rust
impl AppState {
    pub fn usage(&self) -> &UsageTotals {
        self.surface_metrics.usage()
    }

    pub fn context_used_tokens(&self) -> usize {
        self.surface_metrics.context_used_tokens()
    }

    pub fn context_limit_tokens(&self) -> usize {
        self.surface_metrics.context_limit_tokens()
    }
}
```

These public immutable queries are the migration path for the three previously
public mutable fields; they do not expose revision mutation.

- [x] **Step 3: Replace six AppState fields with the owner**

In `AppState`, replace `usage`, `usage_revision`, `context_revision`,
`context_observed`, `context_used_tokens`, and `context_limit_tokens` with:

```rust
pub(crate) surface_metrics: SurfaceMetricsState,
```

Initialize it with `SurfaceMetricsState::default()`. In
`reset_session_projection`, call `self.surface_metrics.reset()` once.

Update `apply_surface_projection_state` to calculate `session_changed` before
changing identity. Reject lower same-session usage revision through the owner;
reset the owner when the session changes; then apply the projection once:

```rust
let session_changed = self.current_session_id.as_deref()
    != Some(projection.session_id.as_str());
if !session_changed
    && self
        .surface_metrics
        .rejects_usage_revision(projection.usage_revision)
{
    return;
}
if session_changed {
    self.surface_metrics.reset();
}
self.current_session_id = Some(projection.session_id.clone());
self.current_session_title = Some(projection.title.clone());
self.surface_metrics.apply_projection(&projection);
```

Keep workflow, Goal, and foreground-operation application unchanged. Rewrite
the debug assertion to call `surface_metrics.assert_matches_projection` and
compare the remaining domains. Do not retain `context_observed` or expose
revision setters/getters to other modules.

- [x] **Step 4: Delete the granular TUI event contract**

Remove these variants from `TuiEvent`:

```rust
UsageUpdated { revision: u64, usage: UsageTotals },
ContextUpdated { used_tokens: usize, limit_tokens: usize },
```

Remove both corresponding `AppState::update` match arms. In
`TuiSurfaceProjection::reduce_typed_batch`, replace usage projection with
`SurfaceEvent::Usage(_) => {}` and remove only the `ContextUpdated` push from
the context arm. Preserve `CompactionStarted` and `Compacted` event
construction and ordering.

- [x] **Step 5: Run the RED tests to GREEN**

Run the two Task 1 commands. Expected: both pass, with usage and idle context
producing no pre-snapshot TUI metric event.

### Task 3: Migrate Rendering, Commands, And Behavior Tests

**Files:**
- Modify: `crates/orca-tui/src/app.rs`
- Modify: `crates/orca-tui/src/slash_command_actions.rs`
- Modify: `crates/orca-tui/src/types.rs`
- Modify: `crates/orca-tui/src/ui.rs`
- Modify if compiler evidence requires: other `crates/orca-tui/src/*.rs`

- [x] **Step 1: Migrate production readers to immutable queries**

Replace direct production reads with the owner queries. Typical replacements:

```rust
state.usage()                 // replaces &state.usage
state.context_used_tokens()   // replaces state.context_used_tokens
state.context_limit_tokens()  // replaces state.context_limit_tokens
```

For arithmetic, bind `let used = state.context_used_tokens();` and
`let limit = state.context_limit_tokens();` once so a single render/command
uses one coherent borrowed observation. Update `app.rs` imports to obtain
`SurfaceProjectionState` from `crate::surface_projection`.

- [x] **Step 2: Rewrite direct-mutation tests through committed snapshots**

In `types.rs`, change `usage_update_allows_compaction_drop_and_rejects_stale_revision`
to construct successive `SurfaceProjectionState` values and deliver only
`SurfaceProjectionSynced`. Assert that revision 10 -> 11 accepts the usage
drop and a later revision 9 snapshot cannot overwrite it.

Remove the old provider-context arbitration cases from
`surface_projection_consistency_reconciles_session_scoped_state`; replace them
with equal-context-revision idempotence, revision-2 compaction application, and
new-session reset/application assertions. All reads use AppState queries.

In `ui.rs` and `slash_command_actions.rs` test modules, add local helpers that
build a complete `SurfaceProjectionState` and call
`state.update(TuiEvent::SurfaceProjectionSynced(...))`. Use those helpers instead
of assigning metric fields. Rename
`provider_context_survives_same_revision_surface_sync_in_footer` to
`equal_revision_surface_sync_keeps_committed_context_in_footer` and remove the
now-deleted granular event.

Extend the legacy restart-context test to apply its real emitted
`SurfaceProjectionSynced` value to a fresh `AppState` and assert all owner
queries. This verifies restart hydration rather than only payload construction.

- [x] **Step 3: Compile and run focused behavior suites**

Run:

```bash
cargo test -p orca-tui typed_usage_projection --lib --locked -- --test-threads=1
cargo test -p orca-tui typed_context_projection --lib --locked -- --test-threads=1
cargo test -p orca-tui surface_projection_consistency --lib --locked -- --test-threads=1
cargo test -p orca-tui context_cell --lib --locked -- --test-threads=1
cargo test -p orca-tui status_line --lib --locked -- --test-threads=1
```

Expected: all focused suites pass with no reference to the deleted event
variants or flat metric fields.

- [x] **Step 4: Verify the ownership/deletion boundary**

Run:

```bash
rg -n 'UsageUpdated|ContextUpdated|context_observed' crates/orca-tui/src
rg -n '\.(usage|usage_revision|context_revision|context_used_tokens|context_limit_tokens)\s*=' crates/orca-tui/src --glob '*.rs'
rg -n 'surface_metrics' crates/orca-tui/src --glob '*.rs'
```

Expected: the first search returns no result; production assignments in the
second search exist only inside `SurfaceMetricsState` (test construction of the
snapshot payload is allowed); the final search shows one AppState owner and its
methods.

### Task 4: Refresh Architecture Evidence And Validator Anchors

**Files:**
- Modify: `docs/production-roadmap.md`
- Modify: `docs/superpowers/specs/2026-08-15-tui-surface-metrics-projection.md`
- Modify if validation reports moved anchors: `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json`
- Modify if reviewed artifacts change: `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.digest.json`

- [x] **Step 1: Update the roadmap with the completed vertical slice**

Add a paragraph after edit-highlight convergence recording that metric commits
now use one reducer-snapshot event, `SurfaceMetricsState` owns local values and
revisions, granular TUI metric events and `context_observed` are deleted,
compaction lifecycle events remain, and external protocols/persistence are
unchanged. Update the convergence module count and current `app.rs`/`types.rs`
line counts after formatting. Keep session/Goal/workflow/operation projection
listed as open; do not claim full surface convergence.

- [x] **Step 2: Run the runtime-surface validator and repair only factual drift**

Run:

```bash
node scripts/validate-runtime-surface-contract.mjs
```

If the private payload/event relocation shifts reviewed `types.rs::UserAction`
anchors, update each reported manifest location to the exact current line.
Do not weaken closed-world inventories, add harmless baselines, or change
runtime source-fact rows for `SurfaceEvent::Usage`/`SurfaceEvent::Context`.

- [x] **Step 3: Recompute reviewed artifact digests structurally**

Run `shasum -a 256` for only the reviewed spec, manifest, and historical plan
listed in the digest file, then update exact SHA-256 values in the digest JSON.
Run the validator again and require success.

- [x] **Step 4: Mark the slice Spec implemented only after gates are green**

Update the new Spec status and self-review with the exact branch/base, RED/GREEN
evidence, preserved compaction semantics, and validator result. Do not mark the
broader TUI convergence or release complete.

### Task 5: Run Full Verification And Independent Review

**Files:**
- Review all changed files from `git diff --stat` and `git diff`

- [x] **Step 1: Run locked compile and full TUI behavior gates**

```bash
cargo check -p orca-tui --tests --locked
cargo test -p orca-tui --lib --locked -- --test-threads=1
cargo test --test tui_pty_contract --locked -- --test-threads=1
```

Expected: exit 0, all TUI library tests and all six root PTY contracts pass.
Existing unrelated warnings must be identified as pre-existing; add no new
warning in changed production code.

- [x] **Step 2: Run validator self-tests and hygiene gates**

```bash
node --test scripts/test-validate-runtime-surface-contract.mjs
node --test scripts/test-validate-windows-platform-boundaries.mjs
node scripts/validate-runtime-surface-contract.mjs
cargo fmt --all -- --check
git diff --check
```

Expected: all commands exit 0.

- [x] **Step 3: Perform a requirement-by-requirement diff audit**

Compare the implementation to every Spec acceptance item. Inspect `git diff`
for a second metric fact source, direct mutation, removed compaction lifecycle,
external protocol change, unrelated cleanup, or unverified claim. Correct any
gap and rerun affected gates.

- [x] **Step 4: Request independent code review**

Provide the reviewer the Spec, plan, base SHA, current HEAD/diff, RED/GREEN
evidence, focused/full results, and explicit questions about partial publication,
revision ordering, session reset, compaction event order, and compatibility.
Resolve every Critical and Important finding and rerun affected tests. Record
Minor findings or close them when low-risk and in scope.

Review found the pre-existing manual delivery deferral, the public Rust field
migration, and missing restart-owner assertion. The final patch documents and
tests snapshot -> `Compacted` -> terminal delivery, exposes public immutable
queries plus the old payload re-export, and hydrates a fresh owner from a real
restart snapshot. Re-review found no remaining Critical or Important issue.

### Task 6: Commit, Rebase, Integrate, And Clean Up

**Files:**
- Stage only this slice's source, tests, docs, manifest, and digest changes

- [x] **Step 1: Create one semantic commit**

```bash
git add crates/orca-tui/src/surface_projection.rs \
  crates/orca-tui/src/types.rs \
  crates/orca-tui/src/app.rs \
  crates/orca-tui/src/surface_client.rs \
  crates/orca-tui/src/slash_command_actions.rs \
  crates/orca-tui/src/ui.rs \
  docs/production-roadmap.md \
  docs/superpowers/specs/2026-08-15-tui-surface-metrics-projection.md \
  docs/superpowers/plans/2026-08-15-tui-surface-metrics-projection.md \
  docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json \
  docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.digest.json
git commit -m "refactor(tui): own surface metrics projection"
```

Adjust the staged file list only for files actually changed by this slice.
Confirm `git status --short` is clean after commit.

- [x] **Step 2: Fetch and rebase latest main**

From the owned worktree, fetch `origin`, then rebase onto current local `main`.
If main advanced in a related area, preserve both current projection behavior
and this single-source owner; do not mechanically choose one side.

- [x] **Step 3: Reverify the rebased branch**

Rerun the focused projection tests, locked TUI check, full serial TUI suite,
root PTY contract, direct validator plus both validator self-tests, formatting,
diff checks, ownership searches, and clean status.

- [x] **Step 4: Fast-forward clean local main and verify the integrated tree**

From the main checkout, require clean status and fast-forward `main` to the
feature commit. Rerun full TUI, root PTY, validators, formatting, and diff checks
on integrated main before cleanup. Do not push, tag, release, or publish an
architecture-only slice.

- [x] **Step 5: Remove only the owned worktree and branch**

After confirming the owned worktree is clean and its HEAD equals integrated
main, remove `.worktrees/tui-surface-metrics-owner` from the main checkout and
delete `codex/tui-surface-metrics-owner`. Leave every unrelated worktree and
branch untouched.

## Plan Self-Review

- Every Spec behavior maps to a task: single commit boundary (Tasks 1-2), owner
  and revisions (Tasks 2-3), compaction/failure/restart behavior (Tasks 1-3),
  compatibility and old-path deletion (Tasks 2-4), and complete verification
  plus review/integration (Tasks 5-6).
- No task introduces a second metrics cache, compatibility wrapper, background
  task, protocol change, or durable format.
- Type names and APIs are consistent: `SurfaceProjectionState` is the inbound
  commit payload, `SurfaceMetricsState` is the AppState owner, and callers use
  `usage()`, `context_used_tokens()`, and `context_limit_tokens()`.
- Placeholder scan: no marker or unspecified test step remains. The remaining
  projection domains are an explicit later
  boundary, not unfinished work inside this slice.
