# Runtime Legacy Terminal Task Reconciliation

## Status

Implemented on `codex/runtime-legacy-terminal-task-reconciliation`, based on
clean local `main` at `bac3c60f5`. This is a runtime and persistence ownership slice.
It is the first safe cold-reconciliation step for pre-surface task records. It
does not make active, approval-blocked, failed/retryable, workflow, subagent,
shell, or monitor registry-only rows visible or actionable, and it does not
authorize a release.

## Problem And Evidence

Recorded runtime threads recover their typed surface ledger and project
`SurfaceSnapshot.tasks` to every TUI attachment. Live runtime operations create
and update typed task patches, then mirror selected state into `TaskRegistry`
for compatibility. Startup also refreshes an existing legacy mirror from an
existing typed task. The reverse path is intentionally absent: a durable
`tasks.json` row created before the typed surface existed is not admitted to the
surface, even when it is an immutable historical completion.

The absence is currently fail-closed and must stay so for any row that could be
controlled. `BackgroundTaskSummary` does not carry a surface operation fence,
background-owner token, interaction grant, thread incarnation, or surface task
revision. Directly mapping those summaries in the TUI would create a second
fact source and could advertise an approval or foreground action that the
runtime cannot authorize.

The runtime type model already contains `TaskPatch::Reconciled`, durable JSONL
encoding for that patch, and reducer checks for identity and revision changes.
There is no production receipt issuer or commit authority for it. Worse, the
ordinary actor permit can currently submit a thread-scoped `Reconciled` event,
and the reducer replaces the entire task vector without requiring every current
typed task to remain. The unused path is therefore not a safe migration
boundary yet.

Classification: architecture and persistence boundary. The persistent
`TaskRegistry` session file is the legacy source authority, the runtime commit
coordinator is the only admission authority, the surface ledger is the durable
post-admission authority, and the TUI remains a projection consumer only.

## User Value And Scope

After a recorded session restarts, a pre-surface main-session task that had
already reached a non-retryable terminal result can appear in the task panel as
historical state. It cannot be stopped, foregrounded, resumed, or answered. The
same session restart must continue hiding every registry row whose ownership or
interaction authority cannot be reconstructed.

This slice:

- issues an opaque reconciliation receipt while holding the persistent
  `TaskRegistry` session lock over one exact canonical record set;
- admits only registry-only `MainSession` rows in `Completed`, `Stopped`, or
  `Cancelled` state;
- requires no worker PID, lease owner, pending provider response, pending tool
  call, or pending approval response on an admitted row;
- converts each admitted row to a revision-one, foreground-neutral,
  interaction-free, operation-free `SurfaceTask`;
- commits one append-only `TaskPatch::Reconciled` batch through a dedicated
  coordinator authority during recorded-thread cold startup;
- prevents the ordinary actor permit from committing reconciliation patches;
- makes reducer reconciliation retain every existing typed task byte-for-byte
  and permits terminal creation only in the constrained historical form;
- relies on the existing final `SurfaceProjectionSynced` snapshot to hydrate
  the task panel; and
- records the remaining active and rich-task migration boundary explicitly.

It does not add a TUI registry read, a compatibility task event, a second task
cache, a new command, a new key binding, or a new task-control fallback.

## Eligibility Contract

An exact persisted `TaskRecord` is eligible only when all of the following are
true at receipt issuance:

1. Its session id equals the recorded surface thread's legacy session id.
2. Its task id parses as `SurfaceTaskId` and no task with that id exists in the
   recovered surface snapshot.
3. Its type is `TaskType::MainSession`.
4. Its status is `Completed`, `Stopped`, or `Cancelled`.
5. It has a recorded completion timestamp.
6. It has no `worker_pid`, `lease_owner`, pending tool call, pending provider
   response, or pending approval response.
7. It is not used to create an operation, background, interaction, workflow,
   or subagent identity.

`Queued`, `Running`, `Paused`, `Stopping`, `ApprovalRequired`, and `Failed` are
ineligible. `Failed` stays hidden because the legacy lifecycle explicitly
permits retrying it. Non-main-session rows stay hidden because a task row alone
cannot reconstruct their workflow/subagent ownership graph.

Ineligible and malformed rows are skipped, not repaired or failed. A skipped
row cannot block a valid recorded session from opening. The receipt records the
complete eligible set under the session lock, including each source
`publication_revision`, and orders it by task id before hashing.

## Receipt And Commit Authority

`TaskRegistry` owns the receipt constructor and keeps receipt fields private.
For persistent registries it acquires the session lock, reloads the canonical
session file, builds the sorted eligible record set, and invokes the cold
reconciliation callback before releasing that lock. Process-local registries
produce no cold receipt.

The callback may only build and commit the surface batch. It must not call back
into `TaskRegistry`, wait on a worker, or publish presentation. Holding the
session lock through surface commit closes the read/commit race: no other
legacy writer can change the receipt-backed rows before the surface ledger has
accepted or rejected the batch.

The runtime commit coordinator exposes a dedicated reconciliation commit
method. It validates all of the following before touching the ledger:

- the receipt session matches the current recorded thread;
- the batch contains exactly one thread-scoped `TaskPatch::Reconciled` event;
- all current surface tasks are retained byte-for-byte;
- appended tasks exactly equal the receipt's eligible registry-only mapping;
- there are no duplicate or colliding task ids; and
- the reconciliation source revision is at least every included surface task
  revision and the receipt's legacy publication horizon.

The ordinary actor permit rejects every `TaskPatch::Reconciled` event. No
caller can bypass the receipt by using `commit_actor_batch`.

## Surface Mapping

Each admitted historical row maps as follows:

| Legacy field | Surface field |
| --- | --- |
| `id` | parsed `SurfaceTaskId` |
| `publication_revision` | receipt source horizon only |
| `task_type=MainSession` | `SurfaceTaskType::MainSession` |
| `Completed/Stopped/Cancelled` | same terminal surface status |
| `description` | `DisplayText` |
| timestamps | matching `UnixMillis` values |
| usage | matching token totals and cost converted by existing `usd_to_micros` |
| result/error/retry/truncation | matching safe presentation facts |

The new surface task always has revision one, `backgrounded=false`, and no
parent operation, background fence, workflow run, subagent, or pending
interaction. These absences are part of its durable non-actionable identity,
not values to be inferred later.

Existing native surface tasks are copied into the reconciliation payload
without modification. Reconciliation is append-only in this slice: omission,
replacement, revision advancement, status transition, or ownership change of
an existing typed task is rejected.

## Lifecycle, Failure, And Restart Semantics

- Normal cold start: recover the surface ledger, owner takeover, and existing
  operation/workflow/provider outcomes first; then issue the locked registry
  receipt and commit any non-empty reconciliation before background-approval,
  Goal-outbox, final mirror, and hub-binding publication.
- Empty or ineligible registry: no surface batch is built and startup proceeds
  without consuming a cursor revision.
- Receipt read failure: startup fails with a typed thread-start error rather
  than treating an unverified task list as empty.
- Invalid/malformed row: that row remains legacy-only and hidden; other valid
  terminal rows may still reconcile.
- Surface validation rejection: no registry state changes and startup fails;
  no partial task projection is published.
- Ledger append/checkpoint failure: the existing bounded semantic-commit retry
  policy applies. An uncertain prepared batch remains owned by the surface
  ledger recovery path.
- Crash after prepare/append: on the next start, ledger recovery may replay only
  the already-recorded append-only terminal reconciliation shape. It does not
  need to fabricate a new receipt for a batch that the prior process already
  admitted under the lock.
- Crash before append: no surface fact exists; the unchanged registry record is
  eligible for a fresh locked receipt on the next start.
- Repeated restart: task-id collision with the already recovered surface task
  makes the row a no-op. No duplicate task or cursor-consuming empty batch is
  created.
- Concurrent legacy writer: it waits on the session lock until commit returns.
  Later writes cannot retroactively alter the committed surface fact.
- Shutdown/cancel/timeout: this slice creates no worker, timer, cancel token, or
  control route. Imported tasks are already terminal and non-actionable.

## Reducer And Recovery Invariants

The reducer keeps replay deterministic without trusting live registry state.
For `TaskPatch::Reconciled` it requires unique ids, a source revision not below
any included task revision, and retention of every current task. A new queued
or running task retains the existing revision-one rule. A new terminal task is
legal only when it is the constrained historical `MainSession` shape defined
above. No terminal task may carry live ownership or interaction identifiers.

Fresh reconciliation requires the opaque receipt authority. Recovery of an
already prepared ledger batch uses a separate exact-shape check against the
pre-batch snapshot. That recovery authority cannot authorize a new batch, an
active task, a failed task, a task omission, or a modified existing task.

## Ownership And Compatibility

`TaskRegistry` remains the source authority for pre-surface persisted records
and existing CLI/server compatibility reads. The runtime surface becomes the
only authority after a row is admitted. The compatibility mirror is not
deleted in this slice and no write direction is reversed for live typed tasks.

`TaskPatch::Reconciled` and its stored JSONL representation already exist, so
the event discriminant and persistence schema do not change. The change begins
using that existing variant in production with stricter reducer and commit
authorization. Existing `tasks.json`, transcript, CLI, JSONL/server, ACP, slash
command, and public task-summary shapes remain readable and unchanged.

The intended user-visible change is additive: eligible historical terminal
main-session tasks can appear after restart. Active/approval/retryable/rich
legacy rows remain hidden. No public Rust field or method is removed.

## Acceptance

1. A RED TaskRegistry test proves a persistent receipt contains only sorted,
   valid `Completed/Stopped/Cancelled` main-session rows and excludes active,
   approval, failed, leased, worker-owned, pending-response,
   missing-completion-time, malformed-id, and non-main-session rows.
2. A RED coordinator test proves ordinary actor commit rejects
   `TaskPatch::Reconciled`, while an exact locked receipt authorizes one
   append-only terminal batch.
3. Reducer tests prove current tasks cannot be omitted or changed, constrained
   terminal creation succeeds, and terminal tasks with live ownership,
   interaction, workflow, or subagent identity are rejected.
4. `recorded_restart_reconciles_registry_only_terminal_main_session_task_once`
   seeds a registry-only completed main-session row and proves the durable
   surface snapshot contains it exactly once with revision one and no
   actionable fences; the ledger contains only one reconciliation after a
   second restart.
5. A TUI restart test proves the same row arrives through
   `SurfaceProjectionSynced` and remains non-actionable; the existing
   registry-only approval regression remains hidden and non-actionable.
6. Repeated restart produces no duplicate or empty reconciliation commit.
   `recorded_restart_rejects_unreadable_terminal_reconciliation_receipt` and
   `recorded_restart_terminal_reconciliation_append_failure_is_bounded_and_non_mutating`
   prove receipt-read and bounded surface-commit failures do not publish an
   unverified task or mutate the registry row.
7. Searches prove TUI production code still has no `TaskRegistry` read or
   `task_summaries()` fallback, and ordinary actor authority cannot admit
   reconciliation.
8. Locked focused runtime tests, runtime-surface reducer/commit/store tests,
   TUI restart/task-projection tests, full serial runtime and TUI library
   suites, root PTY contract, both validator self-tests, runtime-surface
   validator, formatting, and diff integrity pass.
9. Independent review finds no active-task exposure, receipt bypass, task
   deletion, duplicate restart import, lock-order hazard, unsafe recovered
   authority, second TUI fact source, or external compatibility regression.

## Deferred Boundary

The following remain fail-closed and require separate Specs:

- active main-session takeover with a durable surface operation and background
  owner fence;
- approval recovery with a durable interaction, response grant, and exact
  pending provider response;
- failed-task retry identity and idempotency;
- workflow, subagent, shell, and monitor graph reconstruction;
- retirement or narrowing of the legacy compatibility mirror; and
- cross-interface control of reconciled active work.

## Migration, Deletion, And Rollback

The migration order is receipt RED tests, reducer RED tests, dedicated commit
authority, cold-start producer, restart/TUI integration coverage, manifest and
roadmap updates, full gates, review, rebase, and local-main integration. The
ordinary actor reconciliation permission is removed in the same semantic
commit that adds the receipt authority; no permissive intermediate state is
committed.

No data file is rewritten merely to opt into this slice. Reverting the semantic
commit leaves previously appended reconciliation events readable because the
variant already exists, but removes future production imports. No push, tag,
GitHub Release, npm publication, or migration command is authorized.

## Spec Self-Review

The scope is one independently testable ownership transition: immutable legacy
terminal main-session history crosses from a locked registry receipt into the
durable typed surface. Normal, empty, malformed, collision, retryable, active,
approval, failure, crash, restart, concurrency, rollback, compatibility, TUI
projection, and deferred richer-task cases are explicit. The design adds no
second TUI fact source, no actionable identity without a fence, no placeholder,
and no unbounded worker or wait.
