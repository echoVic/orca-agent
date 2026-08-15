# TUI Convergence Slice 17: Foreground And Recoverable Operation Projection Ownership

## Status

Implemented and locally verified on `codex/tui-operation-projection-owner`,
based on clean local `main` at `c073b3a3b` after the session-identity
projection slice was integrated. The branch passed the locked TUI compile,
full serial TUI library suite, root PTY contract, both validator self-tests,
runtime-surface validator, formatting, and diff-integrity gates. CodeRabbit
reviewed the source and roadmap diff with zero issues. The commit was rebased,
fast-forwarded into clean local `main`, and reverified from the integrated
checkout. This slice does not complete the broader TUI convergence or authorize
a release.

The adjacent workflow-task projection boundary was audited and deliberately
deferred. Non-recorded runtime threads still expose live `TaskRegistry`
summaries directly, while `TaskPatch::Reconciled` has no production surface
producer. Making snapshot tasks the only TUI fact before that runtime
reconciliation exists would hide real ephemeral work. This slice changes only
operation facts that are already complete in every `SurfaceSnapshot`.

## Problem And Evidence

`SurfaceProjectionState` carries a snapshot-derived
`foreground_operation_id`, but `AppState` stores that field directly and has no
dedicated owner. Recoverability is worse: the runtime derives it from the same
authoritative snapshot through `SurfaceSnapshot::recoverable_user_operation()`,
then `announce_runtime_ready` emits a second `TuiEvent::RecoveryAvailable`.
`HistoryLoaded`, `TurnStarted`, and `SessionCompleted` also directly clear the
same mutable `recoverable_operation_id` field.

This creates competing sources for the recovery controls. A lifecycle event
can clear a recoverable operation before or after an independent startup
notification, and a rehydrated process has to reconstruct a current runtime
fact through an additional event path. The foreground id and recoverable id
describe one runtime operation observation, so the final accepted surface
snapshot must replace them atomically.

## User Value And Scope

Recovery controls, `/status`, and `/cancel-operation` must target the operation
the active runtime surface currently declares recoverable. This slice:

- adds a private `SurfaceOperationProjectionState` as the sole AppState owner
  of foreground and recoverable operation ids;
- adds the snapshot-derived recoverable id to `SurfaceProjectionState`;
- makes accepted `SurfaceProjectionSynced` payloads and explicit session reset
  the only operation-fact transitions;
- removes `TuiEvent::RecoveryAvailable` and its reducer path;
- replaces the public mutable recovery field with immutable AppState queries;
- keeps recovery-prompt visibility and selection as TUI presentation state,
  driven by a one-time owner effect rather than a second fact event;
- preserves existing action-controller, cancellation, and runtime operation
  fences.

Workflow-task registry reconciliation, operation mutation protocols, stream
lifecycle events, runtime-surface persistence, renderer layout, and side
attachment ownership remain outside this slice. They receive no compatibility
cache or local reconciliation loop.

## State And Transition Contract

`SurfaceOperationProjectionState` starts with no foreground or recoverable id
and no accepted cursor. An accepted projection replaces both values from one
snapshot. The recoverable id is
`snapshot.recoverable_user_operation().map(operation_id)`; it is never
independently inferred by the TUI.

- `SurfaceSessionProjectionState` accepts the envelope before the operation
  owner runs. A stale, contradictory, cross-thread, or cross-incarnation
  snapshot therefore cannot partially update either operation id.
- Lower, cross-identity, and equal-cursor-contradictory operation envelopes are
  rejected. A recoverable id must equal the foreground id in the same envelope.
  Equal accepted snapshots are idempotent. A changed recoverable id produces a
  presentation effect; the same recoverable id at a later cursor does not
  repeat the recovery notice. A transition to `None` clears the recovery prompt
  and selection.
- `SessionProjectionReset` validates its envelope before clearing any
  session-scoped state. Once admitted, it resets the operation owner and then
  applies the new snapshot atomically with the other projection owners.
- `HistoryLoaded`, `TurnStarted`, and `SessionCompleted` retain their
  transcript/status work but do not write foreground or recoverable operation
  facts. The following authoritative snapshot determines those values.
- Renderers and command handlers use immutable `foreground_operation_id()` and
  `recoverable_operation_id()` queries. No production caller can assign an id.

## Lifecycle, Failure, And Restart Semantics

- Startup and reattachment read one runtime snapshot and emit one projection
  envelope. If it declares a recoverable suspended user operation, the accepted
  owner opens the existing recovery controls and presents the existing notice;
  no `RecoveryAvailable` event follows it.
- Resume history may arrive before the snapshot, but it cannot overwrite the
  operation owner. A fresh AppState rehydrated from a real snapshot shows the
  current recovery controls when the runtime still marks the operation
  recoverable.
- Starting or completing an operation changes recovery availability only after
  the runtime commits a surface snapshot. The prompt hides when an accepted
  snapshot carries no recoverable operation.
- Slash recovery and cancel actions read the immutable recoverable id at the
  moment the user invokes them. Their runtime fence, error, and cancellation
  behavior are unchanged.
- Snapshot reads that fail emit the existing projection error and fabricate no
  local operation fact. Rejected snapshots leave both ids and prompt state
  untouched. This owner creates no task, thread, timer, cancellation token, or
  durable write.
- On disconnect the last accepted process-local state remains visible. A
  process restart has no saved local owner; it rebuilds from the runtime
  snapshot. This is current-state rehydration, not an exactly-once notice or
  persistence guarantee.

## Ownership And Compatibility

The runtime surface reducer owns operation state and recovery eligibility.
`SurfaceProjectionState` is the committed snapshot envelope.
`SurfaceOperationProjectionState` is the sole process-local owner of the two
operation ids. `AppState` owns only recovery-prompt presentation derived from
that owner effect.

There is no CLI argument, slash syntax, key binding, server/JSONL or ACP
protocol, runtime surface event, schema, or history-format change. Removing
`TuiEvent::RecoveryAvailable` and replacing the mutable AppState recovery
field are workspace-internal Rust source changes. Workspace callers migrate to
immutable queries in the same commit; retaining a mutable facade would retain
a second fact source. One semantic commit is the rollback boundary.

## Acceptance

1. RED tests prove the runtime snapshot projects recovery eligibility into the
   envelope, startup emits no granular recovery fact, and lifecycle events
   cannot overwrite an accepted operation snapshot.
2. AppState owns exactly one `SurfaceOperationProjectionState`; production code
   has no mutable foreground/recoverable operation id field or assignment.
3. The operation owner atomically covers first hydration, idempotent replay,
   changed recovery ids, recovery removal, rejected stale/cross-session
   snapshots, and explicit session reset.
4. Fresh-process and restored-session tests apply real projected snapshots and
   prove recovery controls and `/status` use the authoritative id.
5. `RecoveryAvailable` has no production sender, TUI event variant, reducer
   path, or compatibility shim. Prompt visibility remains correct after
   accepted recovery and non-recovery snapshots.
6. Focused owner, startup/restart, status-key, slash-cancel, projection
   consistency, and resume tests pass, followed by locked TUI compilation, the
   full serial TUI library suite, root PTY contract, both validator self-tests,
   runtime-surface validator, formatting, diff integrity, and obsolete-path
   searches pass.
7. Independent review finds no duplicate operation fact path, stale recovery
   target, direct field compatibility leak, reset atomicity regression,
   fabricated recovery state, or missing restart/failure coverage.

## Migration, Deletion, And Rollback

The migration order is RED tests, snapshot-envelope completion, owner and
immutable queries, startup/reader migration, lifecycle direct-write deletion,
obsolete event deletion, full verification, independent review, rebase, and
main-only integration. No compatibility event, shadow id, retry worker, or
durable migration remains. A one-commit revert restores the prior internal
adapter path. No push, tag, GitHub Release, or npm publication is authorized by
this architecture-only slice.

## Spec Self-Review

There are no placeholders. Startup, restore, replay, operation start/terminal,
rejection, reset, disconnect, restart, cancellation ownership, compatibility,
and rollback are explicit. The workflow registry gap is deferred at its actual
runtime boundary rather than hidden behind a TUI cache.
