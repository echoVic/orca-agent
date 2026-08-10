# P1.4 Durable Task Supervision

## Status

Proposed for `codex/p1-4-task-supervision`, based on `origin/main` at
`e23b7f86ce12b829c2a09bb670935b60176c1b3b`.

## Problem And Evidence

`TaskRegistry` persists task snapshots, worker PIDs, and process-group
diagnostics, but it has no durable execution owner. `tasks.rs` serializes file
updates with `ExclusiveFileLock`, yet a previously attached process can still
write a stale in-memory record after another process has recovered the task.
`spawn_worker_reaper` can also publish failure after a worker has already been
taken over. A PID is only a process diagnostic and cannot fence stale task
commits.

This is an architecture defect, not a local race: task cancellation, recovery,
and terminal publication have no single cross-process ownership boundary.

## User Value

The TUI task panel and `task_list` describe one durable task state. A background
subagent can be stopped and reaped, a crashed owner can be taken over, and an
old worker cannot overwrite the new owner's terminal result. Users no longer
need to infer ownership from an ambiguous detached process.

## Scope

This slice adds a durable lease to persistent `TaskRegistry` records:

- a registry process has a random owner id;
- a task lease has owner id, monotonically increasing epoch, and expiry;
- only a current, unexpired `(owner_id, epoch)` may publish worker progress or
  a terminal mutation;
- an expired lease may be taken over atomically, incrementing its epoch;
- stop requests are durable control facts, not a resettable cancellation token;
- every persistent task mutation is read-modify-written under the session lock,
  increments a publication revision, and refreshes task-wide readers before
  projection;
- worker reapers use the same fenced publication path and cannot replace a
  terminal written by a newer owner.

The slice applies the lease to async subagent workers first, which is the only
current persistent detached-worker path. Main-session, workflow, shell, and
in-process tasks retain their existing runtime ownership while using the same
task snapshot/publication primitives.

## Non-Goals

- Do not change CLI commands, TUI controls, server method names, JSONL event
  names, or existing persisted transcript formats.
- Do not claim arbitrary external tools can resume after process loss. A task
  with an expired lease is recoverable for supervision and may be explicitly
  taken over; unproven external side effects remain indeterminate.
- Do not introduce a second task database, PID-file authority, detached worker,
  or process-local replacement for the durable lease.

## Ownership And State Model

`TaskRegistry` owns task records; the persistent session task file is the only
cross-process authority. Each persistent registry owns a generated `owner_id`.
`TaskLease` is a typed capability containing task id, owner id, epoch, and
expiry. It is held only in the owning process and is never inferred from PID.

Each persistent task record gains backward-compatible optional lease fields and
a monotonically increasing `publication_revision`. Old records deserialize with
no owner and epoch zero. The first worker may acquire that unowned record; a
recovery process may acquire only after its lease expires. Acquisition, renewal,
fenced publication, stop, and takeover each lock the session file, reload the
current record, validate ownership, persist the new record, then update the
local mirror. A stale owner receives a fenced error and cannot publish.

`request_stop` persists `stop_requested` and `Stopping` before signalling a
local/recovered process. Worker code observes that durable flag when renewing or
polling cancellation. A forced stop after process termination revokes the
lease by advancing its epoch, so any late worker terminal is rejected.

## Normal And Failure Semantics

| Situation | Durable result |
|-----------|----------------|
| Worker starts | It acquires a lease, publishes `Running`, and renews before expiry. |
| Current owner completes/fails | The terminal write validates the lease, clears worker PID, releases ownership, and increments publication revision. |
| Another process sees a valid lease | It may read the full snapshot and request stop, but cannot publish owner progress or terminal state. |
| Owner crashes | Its lease expires; a recovery process atomically takes it over with a larger epoch. |
| Old owner later returns | Renewal and terminal publication fail fenced; the takeover state remains unchanged. |
| Stop races with completion | The terminal that wins the durable fenced transaction is retained; forced stop revokes the prior epoch before publishing `Stopped`. |
| Worker exits without terminal | The reaper may publish failure only while its lease remains current; otherwise it only refreshes the new durable state. |
| Persistence failure | No lease, task state, or publication revision is made visible in memory as committed. |

## Compatibility And Migration

All new persisted fields use serde defaults. Existing `tasks.json` remains
readable and becomes lease-capable on its next write. `BackgroundTaskSummary`
adds an optional/additive `publicationRevision` so TUI/server clients can reject
an older task snapshot without changing existing fields. Event names and task
status vocabulary stay unchanged.

## Acceptance Criteria

1. Two registries opened on one persistent root cannot both hold a task lease.
2. An expired owner can be taken over with a greater epoch, without creating a
   second task record or losing unrelated task fields.
3. A stale owner cannot publish progress, completion, failure, or reaper failure
   after takeover; the current owner state remains visible.
4. A stop request survives process boundaries, reaches an adopted Unix/Windows
   worker through the existing process-group/job controls, and produces one
   terminal task record.
5. `list` and task-status projection refresh the complete durable session task
   snapshot and expose monotonically increasing publication revisions.
6. Legacy task records without lease fields still load; existing lifecycle and
   recovery behavior remains covered.
7. Focused task, async-worker, runtime-host, and TUI task-panel tests pass;
   shared-runtime full gates and a cross-process PTY contract pass before
   release.

## Rollback And Deletion

The change is one reversible release slice. Rolling it back restores the legacy
snapshot behavior; no transcript migration is required. The old unfenced task
write path is deleted in this slice rather than retained as a fallback.
