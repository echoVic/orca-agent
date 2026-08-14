# TUI Convergence Slice 15: Goal Projection Ownership

## Status

Implemented on `codex/tui-goal-projection-owner`, based on clean local `main`
at `3983799b6` after the surface-metrics ownership slice was integrated and
verified. Behavior-first RED tests exposed all four competing fact paths, the
implementation moved Goal value and presentation ownership behind the
cursor-fenced snapshot envelope, and independent re-review found no remaining
Critical, Important, or Minor findings. Fresh pre-integration gates passed:
locked TUI check, 1,050 serial TUI library tests, 6 root PTY contract tests,
both validator self-tests, the direct runtime-surface validator, formatting,
and diff integrity. Operational rebase, local-main integration, integrated
verification, and owned-worktree cleanup are recorded in the final task
handoff because they necessarily happen after this document's semantic commit.
This slice does not mark the broader TUI convergence or a release complete.

## Problem And Evidence

The TUI currently has three production paths that can rewrite the same Goal
fact. A typed `SurfaceEvent::Goal` is reduced into `TuiEvent::GoalUpdated` or
`TuiEvent::GoalCleared`, and the same projected batch then appends
`TuiEvent::SurfaceProjectionSynced` with the authoritative reducer snapshot.
Idle edit, clear, and pause commands emit the granular events directly after
their runtime mutation. `TuiEvent::GoalStatus`, used for `/goal show` and
pre-run feedback, also assigns `AppState.current_goal` from a query result.

`AppState.current_goal` is a public mutable field. The snapshot payload stores
only the lossy `ThreadGoal` display model, not the surface cursor or Goal store
receipt, while `apply_surface_projection_state` gates the whole snapshot with
the unrelated usage revision. Equal-usage snapshots can therefore arrive out
of order and overwrite a newer Goal projection. A removed Goal has no display
object from which to recover a Goal revision, so a Goal-only watermark cannot
be reconstructed after deletion.

This is an architecture and boundary defect. The runtime surface reducer and
its cursor already define the committed fact boundary, but the TUI exposes two
mutation adapters plus a query result as competing local fact sources.

## User Value And Scope

The Goal banner and `/goal` feedback must describe the latest committed runtime
surface state after edit, clear, pause, continuation, resume, or concurrent
projection delivery. An older or duplicate snapshot must not restore a stale
objective/status or repeat a clear acknowledgement. This slice:

- adds one private `SurfaceGoalProjectionState` as the sole AppState owner of
  the displayed Goal, its latest surface cursor, and its last presented cursor;
- adds the snapshot cursor and an optional Goal presentation directive to the
  TUI projection payload;
- makes `SurfaceProjectionSynced` the only production event that changes the
  committed Goal fact;
- turns Goal status queries into presentation-only feedback;
- projects idle edit, clear, and pause success from a fresh authoritative
  surface snapshot at or beyond the mutation's committed cursor;
- removes `TuiEvent::GoalUpdated` and `TuiEvent::GoalCleared` and migrates
  renderers, commands, and tests to an immutable `AppState::current_goal()`
  query;
- retains the existing compact status line, clear acknowledgement, no-progress
  pause warning, and running/idle behavior through an atomic presentation
  directive on the accepted snapshot.

Session identity, metrics, workflow tasks, foreground/recoverable operation
identity, Goal actor scheduling, Goal persistence, and renderer orchestration
remain outside this slice. No compatibility adapter or shadow Goal cache is
added for those later convergence boundaries.

## State And Transition Contract

`SurfaceGoalProjectionState` starts with no Goal, no accepted cursor, and no
presented cursor.

- A projection is comparable only when its thread id and surface incarnation
  match the current cursor. A session projection reset clears the owner before
  a new identity or incarnation is accepted.
- Within one cursor identity, a higher `next_seq` replaces the Goal and cursor.
  A lower sequence is rejected. Equal cursor plus equal Goal is idempotent;
  equal cursor plus a different Goal is rejected as an invariant violation.
- Every accepted `SurfaceProjectionSynced` advances the owner cursor even when
  the Goal is unchanged. Delayed Goal projections therefore cannot overwrite a
  later non-Goal batch.
- A Goal presentation directive is processed atomically with its snapshot.
  `Updated` requires an accepted current Goal and renders the existing compact
  Goal notice. `Cleared` requires an accepted empty Goal and renders the
  existing `Goal cleared.` acknowledgement. One cursor can be presented at most
  once, so duplicate snapshot producers do not duplicate user feedback.
- History/startup hydration uses a projection with no presentation directive,
  preserving its silent state hydration. Typed Goal commits attach the
  directive derived from the final reducer snapshot. Idle mutations attach the
  directive derived from their final post-commit snapshot. If another Goal
  commit wins before that read, the acknowledgement describes the newer
  authoritative state instead of the initiating command's stale intent.
- `GoalStatus(Some/None)` formats query feedback and status exactly as today but
  cannot change the owner. A query cannot replace a committed banner fact.
- Renderers and commands borrow `Option<&ThreadGoal>` from
  `AppState::current_goal()` and cannot mutate the Goal or cursor directly.

## Cancellation, Failure, Retry, And Recovery Semantics

- The owner creates no task, thread, connection, timer, cancellation token, or
  external side effect. Goal operation cancellation, waiting, and resource
  cleanup remain owned by the existing runtime surface and controller.
- Normal Goal-run and resumed Goal-run batches update the owner only after the
  reducer accepts the committed batch. Cancellation, permission rejection,
  timeout, verifier failure, budget stop, and no-progress pause change the
  banner only when the runtime commits the corresponding snapshot.
- Idle edit, clear, and pause first receive a committed mutation reply, detach
  that attachment, then read a fresh surface snapshot. The snapshot must have
  the same thread/incarnation and `next_seq >= change_cursor.next_seq`. If this
  proof cannot be read, the command reports that the mutation committed but its
  TUI projection failed; it does not fabricate a local Goal. A user retry reads
  current runtime state and remains fence-checked by the existing Goal action.
- Cursor gaps, reducer rejection, missing reducer snapshots, stale cursors, and
  contradictory equal-cursor payloads cannot partially change the Goal owner.
- Subscription disconnect keeps the last accepted projection. A later attach
  hydrates from an authoritative snapshot. Process restart starts with an empty
  process-local owner and hydrates from the attachment snapshot.
- Session reset clears the cursor and Goal before the new session snapshot.
  There is no durable UI cache, data migration, or exactly-once presentation
  claim across process restart.

## Ownership And Compatibility

The runtime surface reducer remains the authoritative Goal source.
`SurfaceProjectionState` is the typed committed-snapshot envelope delivered to
the TUI. `SurfaceGoalProjectionState` is the unique process-local Goal owner.
`types.rs` coordinates session reset and presentation effects but contains no
mutable Goal fact field or granular Goal mutation branch.

There is no CLI argument, slash-command syntax, key flow, visible label,
runtime `SurfaceEvent::Goal`, server/JSONL or ACP protocol, Goal SQLite schema,
history format, or persisted session change. Removing `GoalUpdated` and
`GoalCleared` changes the public-but-internal `TuiEvent` Rust surface. Replacing
the public mutable `AppState.current_goal` field is a Rust source change;
callers migrate to the public immutable `current_goal()` method. The
doc-hidden `types::SurfaceProjectionState` re-export remains. This workspace
`orca-tui` 0.1 source migration is accepted because preserving either mutable
field or payload event would preserve a second fact source. A one-commit revert
is the compatibility rollback boundary.

## Acceptance

1. RED behavior first proves that a typed Goal commit ends with one
   `SurfaceProjectionSynced` containing the final Goal and its presentation
   directive, with no preceding granular Goal fact event. Before implementation
   it fails on `GoalUpdated`/`GoalCleared` followed by an unannotated snapshot.
2. AppState contains one `SurfaceGoalProjectionState` and no mutable
   `current_goal` field. Production render/command code uses
   `current_goal()`; tests use projections or a test-only owner helper.
3. A newer cursor updates the Goal, an older cursor is ignored, an equal replay
   is idempotent, a contradictory equal cursor is rejected, session reset
   accepts a new identity, and a presentation cursor is rendered at most once.
4. `GoalStatus` remains visible feedback but does not change the owner.
   Startup/history hydration remains silent.
5. Idle edit, clear, and pause emit authoritative projection snapshots at or
   beyond their committed mutation cursors. Existing restored-session and
   pause-bypass behavior tests assert projection payloads instead of granular
   Goal events.
6. Set/resume/automatic continuation retain Goal notices, no-progress warning,
   cancellation, and `TurnStarted` behavior through typed projection.
7. Production search finds no `GoalUpdated`, `GoalCleared`, or direct
   `current_goal` assignment and no Goal fact mutation outside the owner.
8. Focused gates pass:
   `cargo test -p orca-tui typed_goal_projection --lib --locked -- --test-threads=1`,
   `cargo test -p orca-tui surface_goal_projection --lib --locked -- --test-threads=1`,
   `cargo test -p orca-tui goal_status_is_presentation_only --lib --locked -- --test-threads=1`,
   `cargo test -p orca-tui preloaded_goal_edit_and_clear_restore_the_runtime_surface_before_mutation --lib --locked -- --test-threads=1`,
   and `cargo test -p orca-tui active_goal_pause_bypasses_command_backlog_and_cancels_goal_run --lib --locked -- --test-threads=1`.
9. The full serial TUI suite, root PTY contract, runtime-surface validator and
   self-tests, Windows validator self-tests, locked TUI check, formatting, diff
   integrity, manifest digest, and obsolete-path searches pass.
10. Independent review finds no stale projection, duplicate presentation,
    mutation-without-projection ambiguity, user-feedback regression,
    compatibility leak, or missing reset/restart coverage.

## Migration, Deletion, And Rollback

The migration order is RED projection/owner tests, owner and cursor envelope,
AppState/caller migration, idle mutation projection, granular event deletion,
existing Goal test migration, manifest/docs, focused/full verification, review,
rebase, and main-only integration. The public mutable field and both granular
Goal fact events are deleted in the same semantic commit. No temporary facade
or duplicate event path remains.

One commit revert restores the previous process-local field and granular
events. There is no data migration, durable rollback, external protocol
coordination, push, tag, release, or npm publication in this architecture-only
slice.

## Spec Self-Review

There are no placeholders. Normal mutation/run, cancellation, rejection,
timeout, retry, disconnect, cursor failure, reset, and restart semantics are
explicit. Runtime facts, commit envelope, process-local owner, query feedback,
and renderer reads have distinct responsibilities. Every behavior and old-path
deletion has an executable acceptance gate. The slice is independently
implementable, reviewable, revertible, and compatible with the remaining
session/workflow/operation convergence work.
