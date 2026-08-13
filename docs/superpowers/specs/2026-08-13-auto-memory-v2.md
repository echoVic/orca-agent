# Auto-Memory V2 Design

## Status

Implemented on 2026-08-13. This design supersedes the synchronous
final-response append path documented in
`2026-08-08-auto-memory-governance.md` and its implementation plan.

## Decision

Automatic memory is a durable, asynchronous, project-scoped pipeline. A
successful root turn commits its transcript before it enqueues extraction. The
runtime owns one cancellable worker per persistent interactive session. The
worker converts a bounded redacted turn source into typed candidates, publishes
an authoritative ledger atomically, and lets later turns retrieve only relevant
candidates as internal context.

Manual `/remember` storage remains separate. Automatic extraction never appends
model output to manual `memory.md`.

## Invariants

1. **Durable success precedes learning.** A turn must be a successful completed
   semantic root, pass verification, and durably commit transcript completion.
2. **The job is the recovery boundary.** Provider calls never own the first
   durable write. Enqueue is idempotent by session, turn, and source digest.
3. **One turn is the evidence boundary.** Durable transcript `turn_id` records
   take precedence over process-local conversation cursors during recovery.
4. **Tool output is not memory input.** Only user and assistant content is
   captured; system messages and tool results are excluded.
5. **Provider output is untrusted.** Only Markdown bullets with an explicit
   `user`, `feedback`, `project`, or `reference` category can become candidates.
   Invalid output consumes an attempt and retries; only exact `NOTHING` commits
   a valid zero-candidate result.
6. **The JSONL ledger is authoritative.** Markdown and SQLite search are
   repairable derived views, never the commit boundary.
7. **Recall is bounded and attributable.** Retrieved context includes category,
   age, session, and turn provenance, marks entries as non-instructional claims,
   and instructs the model to verify drift.
8. **Cancellation does not invent failure.** A cancelled claim returns to
   pending without consuming an attempt. A candidate already atomically
   committed remains committed and deduplication makes replay safe.
9. **Expired workers are fenced.** Candidate publication requires the current
   unexpired job lease while holding the job lock. A worker returning after
   expiry or takeover cannot mutate the authoritative ledger.
10. **Corruption fails closed.** Invalid candidate, job, or project metadata is
   preserved rather than overwritten as if it were valid state.
11. **Stateless means no memory.** `HistoryMode::Disabled` starts no worker and
    performs neither automatic capture nor recall.

## Storage Model

Project identity is the SHA-256 of, in priority order:

1. normalized Git `origin`, shared across clones and worktrees;
2. canonical Git repository root;
3. canonical non-Git working directory.

Each project directory contains:

- `project.json`: schema version, unhashed identity, last-seen cwd, update time;
- `memory.md`: explicit project notes;
- `candidates.jsonl`: schema-v2 typed candidates and provenance;
- `auto-memory.md`: derived human projection;
- `index.sqlite3`: derived FTS5 search index bound to a ledger fingerprint;
- `jobs/*.json`: schema-v1 extraction jobs, bounded source, lease, retry state,
  extractor provider/model/prompt version, and commit result.

All replacements use `AtomicWritePolicy::NoFollow`; shared files are guarded by
`ExclusiveFileLock`. Candidate writes re-read under lock before deduplication.

## Worker and Retry Semantics

The worker drains two jobs per command, then requeues work behind already
pending commands when another batch may exist. This prevents an old backlog
from starving while allowing a recall barrier to observe a bounded batch. The
barrier itself waits at most five seconds; recall then uses the last committed
ledger and never turns auxiliary extraction latency into an unbounded foreground
dependency.

A claim owns a ten-minute lease and heartbeats every thirty seconds by wall
clock, including while the provider emits no deltas. A failed heartbeat cancels
and joins the provider request. Provider or persistence failures wait thirty
seconds and retry up to three attempts. Expired running leases are reclaimable.
A newly available provider and auxiliary model may claim an old job; claim
metadata records the actual extractor used so provider configuration changes do
not strand work. Before candidate publication, the worker revalidates and
renews its lease while holding the cross-process job lock; publication and the
committed job transition happen inside that fencing boundary. Job-store and
lock failures schedule another worker attempt after thirty seconds rather than
requiring a later session wake.

The active queue is capped at 64 jobs per project. Committed and retry-exhausted
jobs do not consume active capacity. The worker has no tools, MCP registry, or
external tools.

## Privacy and Bounds

Each captured message is redacted before enqueue, capped at 8 KiB, and the turn
source is capped at 32 KiB and 40 messages. The extractor returns at most eight
candidates, each at most 600 bytes. Secret-bearing candidates are rejected
rather than semantically altered by redaction. Candidate deduplication requires
normalized exact equality; token-overlap heuristics cannot discard a correction
whose small textual difference changes the fact's meaning.

Candidate retention is 128 records. The projection shows 64 records. Committed
job retention is 128. Recall returns at most six candidates and 3,072 bytes.

## Retrieval Semantics

Candidate selection searches normalized tokens through SQLite FTS5 and orders
matches by BM25 and record time. The index is rebuilt from the ledger whenever
its fingerprint is missing or stale; corrupt derived databases are replaced.
Any index error falls back to normalized lexical overlap plus recency. Recall is
refreshed only for a new turn; continuations keep the original snapshot. Recall
is represented by `InternalContextKind::Memory`, so it is rendered to the
provider but never added to transcript messages or compacted as user
conversation.

## Controls

`auto_memory` defaults to `true` in file configuration. Setting it to `false`
disables worker startup, capture, and recall for new sessions without touching
manual memory or deleting stored state.

Destructive clearing is intentionally not routed around the typed runtime
surface. Until memory deletion has a revisioned, idempotent command and receipt,
users disable auto memory, stop project sessions, inspect `project.json`, and
remove the exact project directory recoverably.

## Verification Matrix

The owning tests cover:

- config default and explicit wiring;
- memory internal-context replacement and prompt budget;
- strict provider categories, filtering, secret rejection, deduplication, and
  correction preservation, and projection repair;
- FTS index creation, missing-index rebuild, corrupt-index repair, and lexical
  fallback;
- current-turn formatting and durable `turn_id` recovery;
- project identity across clones, worktrees, and non-Git paths;
- origin credential/query stripping before identity metadata publication;
- project metadata auditability and corrupt-evidence preservation;
- metadata revalidation after cross-process lock contention;
- durable enqueue, claim, lease, cancellation, commit, provider/model takeover,
  silent-provider heartbeat, stale-worker fencing, infrastructure retry, queue
  capacity, and retry exhaustion;
- completed root capture and verifier-failed exclusion;
- disabled recall and continuation snapshot stability.
