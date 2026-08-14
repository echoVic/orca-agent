# TUI Convergence Slice 14: Surface Metrics Projection Ownership

## Status

Implemented and locally verified on `codex/tui-surface-metrics-owner`, based on
local `main` at `1983e9cba` after edit-highlight ownership was integrated and
verified. Both behavior tests first failed against the granular metric events,
then passed after their deletion. The locked TUI check, 1,042 serial TUI tests,
six root PTY contracts, both validator self-tests, both direct validators,
formatting, diff integrity, and obsolete-path searches pass. Context compaction
lifecycle construction remains before the appended reducer snapshot; the
existing manual-compaction sender commits that snapshot before its deferred
`Compacted` notice and terminal event. Independent review found and verified the
compaction-order, Rust API migration, and restart-hydration corrections; no
Critical or Important finding remains. The reviewed commit was rebased,
reverified, fast-forwarded to clean local `main`, and its owned worktree and
branch were removed. This status does not mark the broader TUI convergence or a
release complete.

## Problem And Evidence

The typed TUI surface currently projects usage and context through two
production fact paths for every relevant commit. `TuiSurfaceProjection` first
turns `SurfaceEvent::Usage` and `SurfaceEvent::Context` into granular
`TuiEvent::UsageUpdated` and `TuiEvent::ContextUpdated` values, then
`project_typed_batch` appends `TuiEvent::SurfaceProjectionSynced` from the
authoritative reducer snapshot. `AppState::update` mutates the same six metric
facts from both paths.

`AppState` therefore stores usage, usage revision, context revision, an
`context_observed` arbitration flag, used tokens, and limit tokens as separate
fields. The special flag exists to decide whether a later same-revision
snapshot may overwrite the preceding granular context event. Production search
finds no other sender for the granular metric events: the duplicate source is
inside the typed projection itself.

This is an architecture-boundary defect. Runtime surface reduction already
commits one coherent snapshot at a cursor boundary, but the TUI exposes partial
metric mutations before that boundary and then reconciles them with a second
copy. The roadmap names this projection duplication as the remaining core of
TUI/runtime convergence.

## User Value And Scope

The footer and `/status` must show one coherent usage/context observation after
resume, compaction, retries, and attachment replacement. A rejected or
incomplete batch must not leave a partial metric update. This slice:

- adds private `SurfaceMetricsState` in `surface_projection.rs` as the sole
  AppState owner of usage, usage revision, context revision, used tokens, and
  limit tokens;
- replaces the six AppState fields, including `context_observed`, with one
  aggregate and immutable AppState queries;
- makes `SurfaceProjectionSynced` the only production TUI event that changes
  metric facts;
- deletes the granular `UsageUpdated` and `ContextUpdated` TUI event variants
  and their reducer paths in the same slice;
- keeps context compaction lifecycle events because they drive visible running
  status and completion notices rather than duplicate metric facts;
- moves the projection snapshot payload beside its reducer/owner and migrates
  render, command, and tests to the read-only metric queries.

Session identity, workflow-panel tasks, Goal projection, foreground/recoverable
operation identity, transcript streaming, and renderer orchestration remain
outside this slice. They continue to use the existing
`SurfaceProjectionSynced` payload and have explicit later deletion boundaries;
this slice does not wrap them in another adapter.

## State And Transition Contract

`SurfaceMetricsState` starts with default zero usage, no usage revision, no
context revision, and zero used/limit tokens.

- A committed `SurfaceProjectionSynced` for a new session applies its complete
  usage and context facts.
- For the same session, a snapshot with a lower usage revision is rejected with
  the rest of the stale projection exactly as today. Equal or newer usage
  revision is idempotently applied.
- Context facts apply only when their revision advances. Equal-revision replay
  is idempotent; an older context revision cannot restore pre-compaction values.
- A session projection reset clears the entire metric owner before any new
  snapshot, so revisions from the previous session cannot fence the new one.
- Renderers and commands borrow immutable usage/context values. They cannot
  write revisions or values directly.
- `SurfaceEvent::Usage` and idle `SurfaceEvent::Context` produce no standalone
  TUI metric event. After reduction, the batch-level projection snapshot is the
  single metric mutation boundary.
- Running/completed compaction continues to construct its dedicated lifecycle
  event before the final projection sync in the projected batch. The existing
  manual-compaction client deliberately defers `Compacted` delivery until the
  operation boundary, yielding snapshot -> `Compacted` -> terminal so the
  completion notice observes committed metric facts.

## Cancellation, Failure, And Recovery Semantics

- The owner creates no task, thread, connection, timer, cancellation token, or
  external side effect. Runtime operation ownership is unchanged.
- Cancellation, permission rejection, tool failure, and timeout change metrics
  only if the runtime commits a new authoritative surface snapshot. They cannot
  fabricate a local metric delta.
- Cursor gaps, reducer rejection, and missing reducer snapshots return an error
  before the TUI receives a metric mutation. There is no granular partial state
  to roll back.
- Replayed snapshots are idempotent by revision. A stale same-session snapshot
  remains rejected; a new-session reset clears the revision fence.
- On surface disconnect or process restart, the process-local owner resets and
  is hydrated from the attachment snapshot. No persistence schema, replay log,
  or exactly-once claim is added.

## Ownership And Compatibility

The runtime surface reducer remains the authoritative source of usage and
context. `SurfaceProjectionState` is the typed commit snapshot delivered to the
TUI. `SurfaceMetricsState` is the unique local projection owner used by
AppState. `types.rs` coordinates session reset and the other snapshot domains,
but contains no individual metric fact or granular metric-event branch.

There is no CLI argument, key flow, visible label, runtime surface event,
server/JSONL or ACP protocol, history format, SQLite schema, or persisted
session change. Removing `TuiEvent::UsageUpdated` and
`TuiEvent::ContextUpdated` changes the public-but-doc-hidden reducer-internal
adapter contract. Replacing the three public mutable AppState metric fields is
also a Rust source change: callers migrate `state.usage` to `state.usage()`, and
the two context fields to their same-named immutable methods. The old
`types::SurfaceProjectionState` path remains as a doc-hidden re-export. This
workspace-internal `orca-tui` 0.1 API tradeoff is accepted because retaining
mutable public fields would preserve a second metric fact source; one-commit
revert is the rollback boundary. Runtime `SurfaceEvent::Usage` and
`SurfaceEvent::Context` remain unchanged. No compatibility wrapper, shadow
metric cache, or second revision watermark remains.

## Acceptance

1. RED behavior tests first prove that reducing typed usage and idle-context
   patches emits no granular TUI metric event; before implementation they fail
   because `UsageUpdated` and `ContextUpdated` are present.
2. A batch that needs metric projection still ends with one
   `SurfaceProjectionSynced` reducer snapshot through the existing typed surface
   client path. Context compaction lifecycle events remain visible; manual
   compaction preserves snapshot -> `Compacted` -> terminal delivery.
3. AppState contains one `SurfaceMetricsState` field and none of `usage`,
   `usage_revision`, `context_revision`, `context_observed`,
   `context_used_tokens`, or `context_limit_tokens` as individual fields.
4. The TUI event model and reducer contain no `UsageUpdated` or
   `ContextUpdated` variant/path. Production code outside
   `surface_projection.rs` has no direct metric mutation.
5. Snapshot behavior covers initial and restart hydration, compaction drops,
   lower-revision rejection, equal-revision replay, context revision advance,
   and session reset. Footer and `/status` rendering remain behaviorally
   unchanged.
6. Focused gates pass:
   `cargo test -p orca-tui typed_usage_projection --lib --locked -- --test-threads=1`,
   `cargo test -p orca-tui typed_context_projection --lib --locked -- --test-threads=1`,
   `cargo test -p orca-tui surface_projection_consistency --lib --locked -- --test-threads=1`,
   and `cargo test -p orca-tui context_cell --lib --locked -- --test-threads=1`.
7. The full serial TUI suite, root PTY contract, runtime-surface validator and
   self-tests, Windows validator self-tests, locked TUI check, formatting, diff
   checks, and obsolete-path searches pass.
8. Independent review finds no partial metric publication, stale-revision
   regression, compaction ordering loss, compatibility leak, or missing
   failure/restart coverage.

## Migration, Deletion, And Rollback

The owner, snapshot-type relocation, AppState/caller migration, granular event
deletion, tests, validator verification, and roadmap update land in one semantic
commit. The six old AppState fields and both granular TUI event paths are
deleted in the same slice. The remaining session/goal/workflow/operation
snapshot fields are the next projection convergence boundary; this slice adds
no temporary facade for them.

One commit revert restores the previous process-local layout and duplicate
adapter events. There is no data migration, durable rollback, or external
protocol coordination.

## Spec Self-Review

There are no placeholders. Normal commits, cancellation/rejection/timeout,
cursor or reducer failure, replay, compaction, disconnect, session reset, and
restart are explicit. Runtime facts, the commit snapshot, local metric state,
and renderer reads have distinct owners. Every behavior and deletion condition
has an executable acceptance gate, and the slice is independently implementable,
reviewable, revertible, and compatible with the remaining projection work.

Implementation review confirms one `SurfaceMetricsState` field in `AppState`,
no granular metric event or `context_observed` path, immutable renderer/command
queries, revision-fenced replay, and session-reset hydration. The runtime
surface manifest and reviewed digests did not drift, so validation required no
baseline or digest change.

Independent review additionally verified manual delivery as
`CompactionStarted` -> final snapshot -> `Compacted` -> terminal, preserved the
doc-hidden payload path with public immutable field-migration queries, and
applied a real restart snapshot to a fresh owner. All Important findings were
resolved and the targeted regressions passed.
