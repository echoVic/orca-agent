# Runtime Legacy Active Task Adoption

## Status

Implemented on `codex/runtime-legacy-active-task-adoption`, based on clean local
`main` at `baeca39b41`. This is a runtime and persistence ownership slice. It
does not authorize a release.

## Problem And Evidence

Recorded runtime threads recover their typed surface ledger before the TUI
attaches. A live typed background main-session owns a durable operation,
generation fence, background-owner token, and revisioned `SurfaceTask`; the
legacy `TaskRegistry` row is only a compatibility mirror. A pre-surface
registry-only row has none of those authorities.

The previous terminal-reconciliation slice safely imports immutable historical
`Completed`, `Stopped`, and `Cancelled` rows. It deliberately rejects
`Running`, approval-blocked, failed/retryable, and rich task rows. After a
restart, a registry-only `Running` main-session therefore remains absent from
`SurfaceSnapshot.tasks`. TUI stop and foreground commands cannot address it
because both commands require a surface task with `parent_operation`, and
foreground additionally requires a matching background-owner fence.

The old row is not resumable. It contains presentation metadata and partial
usage, but no admitted input capsule, live provider suspension, resident
controller, or exact continuation boundary. Treating it as still executing
would be false. Reading it directly from the TUI would also restore the second
fact source that the typed surface migration removed.

The runtime already has the truthful cold-recovery rule for a started or
transferred non-replayable generation whose live capsule is unavailable:
`StopAndFinalizeRuntimeRestart`, producing
`OperationTerminal::AbortedByRuntimeRestart`. Existing terminal task
reconciliation then converts its associated running main-session task to
`Stopped` and mirrors that result into `TaskRegistry`.

Classification: architecture and persistence boundary. `TaskRegistry` is the
legacy source authority before adoption, the commit coordinator is the only
admission authority, the surface ledger is authoritative after adoption, and
the TUI remains projection-only.

## User Value And Scope

After a recorded session restarts, a safe pre-surface `Running` main-session is
no longer silently missing or falsely left active. The runtime durably records
which operation and background owner adopted it, applies the same restart
terminalization used for native typed operations, and exposes one stopped task
through every surface projection.

This slice:

- issues an opaque active-adoption receipt while holding the persistent
  `TaskRegistry` session lock over one canonical record set;
- admits only registry-only `MainSession` rows in exact `Running` state with no
  live worker, lease, pending interaction, durable provider outcome, stop
  request, or residual result/error/tool state;
- admits the receipt only when the recovered surface has no existing
  foreground, queued, historical, or background operation lineage;
- reconstructs one canonical non-replayable user-turn operation, generation,
  running surface task, transfer, and background-owner fence per admitted row;
- commits all missing receipt-backed adoptions in one recorded batch through a
  dedicated coordinator authority;
- immediately runs the existing cold-operation recovery and terminal-task
  reconciliation paths;
- leaves the legacy row unchanged until the surface operation has durably
  terminalized and the surface task terminal projection has committed;
- remains idempotent across restart and partial-append recovery; and
- keeps approval recovery, retryable failure, other active phases, rich task
  graphs, and cross-interface control of genuinely resumable work out of scope.

It adds no TUI registry read, command, key binding, protocol message, public
Rust API, or persistent legacy task field.

## Eligibility Contract

An exact persisted `TaskRecord` is eligible at receipt issuance only when all
of the following are true:

1. The registry is persistent and its session id is the recorded surface
   thread's legacy session id.
2. The task id parses as `SurfaceTaskId`.
3. The type is `TaskType::MainSession` and the status is exactly
   `TaskStatus::Running`.
4. `started_at_ms` is present and `completed_at_ms` is absent.
5. `worker_pid`, `lease_owner`, and `lease_expires_at_ms` are absent.
6. `stop_requested` is false and the in-memory cancellation token is not part
   of receipt authority.
7. `tool`, `pending_tool_call`, `pending_tool_approval_response`, and
   `pending_provider_response` are absent.
8. No durable typed-provider outcome exists for the task id.
9. `result` and `error` are absent.

The runtime additionally excludes any receipt record whose task id already
exists in the recovered surface snapshot. Existing typed state always wins.
Because a legacy task row carries no typed operation id, the runtime cannot
distinguish a truly registry-only row from the compatibility mirror of a typed
foreground, finalizing, historical, or background operation that has not
materialized a surface task. If any operation lineage already exists in the
recovered snapshot, startup therefore skips the entire active-adoption receipt
without mutation and continues the existing typed recovery path. This
fail-closed rule is also enforced by fresh and prepared commit authority.

`Queued`, `Paused`, and `Stopping` are not mapped to a fake started generation.
`ApprovalRequired` requires its own durable interaction and response grant.
`Failed` remains retryable under its existing identity. Workflow, subagent,
shell, and monitor rows require their full ownership graphs. Malformed or
ineligible rows are skipped without mutation and do not block session startup.

## Receipt And Lock Boundary

`TaskRegistry` owns crate-private receipt and record types with private fields.
For a persistent registry, it acquires the existing session lock, reloads the
canonical session file, reloads the typed-provider-outcome map under the same
session serialization boundary, filters and sorts eligible records by task id,
computes a canonical SHA-256 digest, and invokes a callback before releasing
the session lock. A process-local registry returns no receipt. Persistence
failure is an error, never an empty receipt.

Recorded Resume session construction attaches to the persistent task registry
without running the older blanket interrupted-task rewrite before surface
bootstrap. New recorded sessions and explicit standalone registry recovery keep
their existing behavior. This ordering lets the receipt observe the durable
pre-surface row; ineligible resume rows remain unchanged for their owning
recovery paths rather than being mutated as a side effect of opening the
session.

The receipt contains only immutable mapping inputs: session id, canonical
record set, source publication revisions, a positive publication horizon, and
the digest. It exposes no `TaskControl`, worker handle, cancellation token,
lease mutation, or writable `TaskRecord`.

While the callback holds the session lock it may read the recovered surface,
build the adoption batch, and commit it. It must not call back into
`TaskRegistry`, await a worker, run provider code, or publish TUI events. This
closes the legacy read/surface commit race without introducing a reverse lock
order.

## Canonical Adoption Batch

When the recovered surface contains no operation lineage, the runtime appends
the following five events for every receipt-backed record missing from the
surface, in this exact order within one recorded commit:

1. operation-scoped `OperationPatch::Requested`;
2. operation-scoped `OperationPatch::Admitted` with generation zero;
3. generation-scoped `OperationPatch::GenerationStarted`;
4. thread-scoped `TaskPatch::Upserted` with a running background task; and
5. generation-scoped `OperationPatch::GenerationTransferred` naming that task
   and its background fence.

Each operation has a fresh UUIDv7 operation/request/logical-turn identity, a
current-owner reservation sequence, and a canonical intent:

- `OperationOrigin::TuiUser` and `OperationKind::UserTurn`, preserving the only
  legacy main-session provenance that can be stated without inventing an
  external caller;
- `Replayability::NonReplayable { reason: Missing, live_capsule: Unavailable }`;
- current settings revision and policy epoch with a `Current` settings receipt;
- queue busy disposition, suspend-until-explicit-control interrupt settlement,
  publish-after-admitted legacy visibility, and no required capabilities; and
- the SHA-256 digest of the fixed domain
  `orca.runtime.legacy-active-task-adoption.v1` as one capability fingerprint
  shared by the operation, generation, and start witness.

The generation is initial attempt zero, has `NotApplicable` input, no
predecessor or goal identity, and uses the current thread id and owner epoch.
The start witness binds the current settings, policy, replayability digest, and
capability fingerprint.

The task is revision one, `MainSession`, `Running`, backgrounded, and linked to
the operation and exact background fence. It preserves description,
creation/start timestamps, partial usage, retry count, and truncation. It has
no completion, result, error, workflow, subagent, or interaction identity. The
background owner token is random and opaque; authorization validates its exact
equality across the task and transfer rather than deriving or exposing it.

Multiple records are represented by repeated five-event groups in sorted task
order. Each group transfers its admitted foreground operation before the next
group is admitted, so reducer foreground exclusivity remains valid. If no
receipt record is missing from the surface, no batch is committed.

## Commit Authority

`RuntimeCommitCoordinator` exposes a crate-private active-adoption commit
method. Fresh authority requires all of the following:

- a valid receipt whose session matches the current recorded thread;
- recorded persistence and the current exclusive owner epoch;
- exactly five events per missing receipt record, with no other event family;
- sorted, unique task identities and one-to-one operation, generation, task,
  and background-fence relationships;
- exact receipt-derived task presentation fields, with the receipt publication
  horizon validated as part of the receipt digest;
- the canonical operation, generation, replayability, settings, capability,
  and ordering shape above; and
- no existing foreground, queued, historical, or background operation lineage,
  plus no collision with an existing task identity.

Ordinary actor authority cannot commit this receipt-backed multi-operation
shape. Recovered prepared-batch authority validates the same constrained event
shape without needing to reconstruct the old receipt, so a crash after prepare
cannot turn a valid adoption into an unrecoverable thread. It may only append
new running main-session ownership; it cannot alter or remove current tasks or
operations.

The reducer's existing live operation and task transitions remain unchanged.
The new authority does not make `TaskPatch::Reconciled` capable of active
ownership and does not broaden the terminal reconciliation authority.

## Recovery And Mirror Order

Recorded cold startup performs these steps after materializing the new thread
owner and before exposing the surface handle:

1. if no typed operation lineage exists, commit any missing active-adoption
   batch under the legacy session lock; otherwise skip active adoption;
2. enumerate all nonterminal surface operations, including newly adopted ones;
3. apply existing interaction/capability recovery and `recover_operation`;
4. observe `StopAndFinalizeRuntimeRestart` for each adopted unavailable live
   capsule and commit `AbortedByRuntimeRestart`;
5. run existing terminal main-session task reconciliation, committing the task
   transition from `Running` to `Stopped`; and
6. only then mirror the stopped terminal state into `TaskRegistry`.

The final surface task keeps its parent operation and background-owner fence as
historical ownership evidence. It is terminal and therefore cannot be stopped
or resumed as live work. Existing projection code displays it from the surface
snapshot; no TUI special case is introduced.

## Crash Consistency And Failure Semantics

- Failure before the adoption commit leaves the registry row unchanged and no
  surface identity visible.
- A bounded append/checkpoint failure rejects thread startup and retains the
  exact prepared batch for existing ledger recovery rules; it does not mutate
  the registry row.
- A crash after adoption but before operation recovery leaves one durable
  running task with complete operation/background ownership. The next owner
  skips duplicate adoption, recovers that operation, and terminalizes it.
- A crash after operation terminalization but before task projection is closed
  by existing terminal main-session reconciliation.
- A crash after task projection but before legacy mirror mutation is closed by
  startup mirror reconciliation.
- Repeated restarts never create a second surface task or operation for an
  already adopted task id.

Startup does not continue with a partially accepted semantic batch. Retries are
bounded by `SURFACE_SEMANTIC_COMMIT_RETRY_ATTEMPTS`.

## Compatibility

No public protocol request/event, CLI syntax, TUI action, task JSON schema,
surface event variant, or external API changes. The surface ledger only uses
existing operation and task events. Older ledgers remain readable. Rollback to
an older binary sees the added events as ordinary existing variants; the
legacy row has already converged to `Stopped` only after the durable terminal
projection committed.

The new receipt and commit entrypoint are crate-private. The runtime-surface
contract manifest must inventory their ownership paths without weakening any
existing forbidden mutation or TUI boundary rule.

## Verification And Acceptance

The slice is complete only when all of the following are proven:

1. Receipt tests include eligible running rows plus queued, paused, stopping,
   approval, failed, rich, leased, worker-owned, pending, outcome-bearing,
   stopped-requested, missing-start-timestamp, and malformed-id negatives.
2. Commit tests prove exact five-event authorization, receipt substitution and
   omission rejection, relationship/fence mismatch rejection, existing
   operation-lineage rejection, actor rejection, and constrained prepared-batch
   recovery.
3. A recorded restart test starts with one registry-only running main-session,
   observes one stopped surface task with one terminal parent operation and a
   historical background fence, verifies
   `AbortedByRuntimeRestart`, verifies the legacy row is stopped, and proves a
   second restart adds no duplicate adoption events.
4. An injected append-failure test proves bounded startup failure and a
   non-mutated running legacy row, then proves later recovery succeeds once.
5. The previous exclusion test still hides approval-required, failed, queued,
   paused, stopping, and rich tasks.
6. Existing suspended and finalizing operation restart tests prove their
   Running compatibility mirrors are not re-adopted.
7. Searches prove TUI production code has no `TaskRegistry` read or task-summary
   fallback and no new external contract surface appears.
8. Locked focused tests, complete runtime-surface reducer/commit/store targets,
   full serial runtime and TUI library suites, root PTY contract, compiler,
   validator self-tests, runtime and Windows validators, formatting, and diff
   integrity pass.
9. Independent review finds no fabricated continuation, missing fence,
   receipt bypass, task deletion, duplicate restart identity, lock inversion,
   unsafe recovered authority, TUI second fact source, or compatibility drift.

## Deferred Boundary

Separate Specs remain required for:

- queued, paused, and stopping legacy main-session phase reconstruction;
- approval recovery with a durable interaction, response grant, and exact
  pending provider response;
- failed-task retry identity and idempotency;
- workflow, subagent, shell, and monitor graph reconstruction;
- cross-interface control of genuinely resumable recovered work; and
- retirement or narrowing of the legacy compatibility mirror.

## Rollback

Rollback removes the active receipt owner, dedicated commit authority, startup
producer, tests, validator inventory, and roadmap entry together. It must not
delete committed surface events or rewrite legacy task files. A rolled-back
binary continues to replay the existing event variants and sees the terminal
task history already committed by this slice.
