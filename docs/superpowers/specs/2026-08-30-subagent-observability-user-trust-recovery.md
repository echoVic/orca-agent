# Subagent Observability, User Trust, And Recovery Architecture

## Status

Proposed on `feat/subagent-observability`, based on `main` at `020502d96`
(`v0.4.6`). This is a coordinated runtime, persistence, protocol, TUI, CLI,
and documentation refactor. Backward compatibility is explicitly out of scope.
Old ambiguous adapters and persisted records fail closed; source transcripts are
never deleted or rewritten as part of that decision.

## Outcome

Orca must make delegated work understandable and controllable from the parent
session. A user can see a child start immediately, follow its current phase and
tool activity, expand the task tree, inspect the latest durable child transcript,
answer a permission request for the exact child operation, and understand what
was actually verified at completion. A detached child continues to publish the
same facts across parent reconnects and runtime restarts without replaying an
external side effect whose outcome is unknown.

The same release must make first run and diagnosis literal: the package is
`@blade-ai/orca`, the product origin is `orcaagent.dev`, `orca doctor` is a real
read-only command, and public documentation describes only implemented commands,
flags, storage, trust, and sandbox behavior.

## Problem

Orca currently has most of the individual pieces but not the end-to-end
contract:

- child loops already identify turn, streaming, tool, and usage activity, but
  the production sync and async paths write child events to `io::sink()`;
- `SubagentPatch::Started`, `Progress`, and `Completed` exist, but no production
  runtime owner publishes them;
- detached workers persist only a latest `TaskRegistry` status and cannot relay
  ordered activity to a reattached parent;
- the TUI runtime projection discards subagent activity and renders a flat task
  list even though the registry has parent relationships;
- permission requests carry strong operation and tool digests but not the child
  task, subagent attempt, or activity revision that caused the request;
- TUI permission answers collapse a typed grant into `bool`, while ACP and JSONL
  have different session-grant ownership;
- verifier output exists in the surface but terminal UX does not present one
  stable completion-proof contract;
- catalog and search can hide or abort on a corrupt session instead of isolating
  it; and
- website quickstarts document obsolete package names, domains, commands,
  flags, and configuration.

This is one user-trust problem. Activity, permission, completion, and recovery
facts must share durable identities and one runtime authority.

## Non-Goals

- Exposing private chain-of-thought, hidden provider reasoning tokens, raw
  authorization headers, secrets, unrestricted tool output, or sensitive paths.
- Letting the TUI read `TaskRegistry`, continuation files, relay files, or raw
  session JSONL directly.
- Replaying an interrupted tool merely because the process restarted.
- Making `orca doctor` provision a sandbox, mutate trust, write credentials,
  start MCP servers, or call a paid provider.
- Preserving the old TUI `Permission(bool)` protocol, adapter-owned session
  grants, obsolete CLI aliases, or permissive malformed-history behavior.

## Architectural Rules

### 1. One source event, one projection owner

All child execution paths emit the same typed `SubagentActivityEvent`:

```rust
pub struct SubagentActivityEvent {
    pub schema_version: u16,
    pub task_id: SurfaceTaskId,
    pub subagent_id: SurfaceSubagentId,
    pub attempt_id: AgentAttemptId,
    pub source_sequence: u64,
    pub occurred_at: UnixMillis,
    pub owner: SubagentActivityOwner,
    pub payload: SubagentActivityPayload,
    pub digest: Sha256Digest,
}

pub enum SubagentActivityPayload {
    Started { description: DisplayText },
    PhaseChanged { phase: SurfaceSubagentPhase, turn: Option<u32> },
    ToolStarted { call_id: ToolCallId, name: ToolName, target: Option<DisplayText> },
    ToolCompleted { call_id: ToolCallId, status: SurfaceToolTerminalStatus,
                    summary: Option<DisplayText> },
    Usage { totals: UsageTotals },
    CheckpointPublished { checkpoint_revision: u64 },
    Completed { status: SurfaceSubagentTerminalStatus,
                output: Option<DisplayText>, error: Option<DisplayText>,
                usage: Option<UsageTotals> },
}
```

`task_id` is the durable delegated-work identity. `subagent_id` identifies the
surface projection for that task. `attempt_id` fences retries and takeovers.
`source_sequence` is contiguous within one attempt and the digest makes retries
idempotent. Tool-call identity is structured, not embedded only in display text.

The child loop emits phase labels and user-visible assistant/tool facts. It does
not emit hidden reasoning content. A `Reasoning` phase means only that the model
is working.

The thread actor is the only owner allowed to convert source activity into
`TaskPatch` and `SubagentPatch` commits. The runtime surface ledger, reducer,
projection, cursor, replay, and subscriber gap protocol remain the only live UI
fact source. `TaskRegistry` is a durable task/lease index and latest-state mirror,
not a second presentation state machine.

### 2. Sync and detached delivery use one contract

`ChildAgentActivityObserver` becomes a fallible `ChildAgentActivitySink`. Every
complete child loop emits started, phase/tool/usage/checkpoint progress, and one
terminal event through that sink.

- A synchronous child uses an actor-owned ingress. Each accepted event is
  validated and committed to the runtime surface before the sink acknowledges
  it.
- A detached worker cannot own a surface commit permit. It appends the same
  event envelope to a durable relay under its current task lease. A thread-actor
  relay drainer validates the lease/attempt and commits events through the same
  ingress used by synchronous children.

The worker must not hold a parent `SessionWriter`, surface coordinator, or TUI
channel. The parent must not poll `TaskRegistry` for UI activity.

### 3. The detached relay is a delivery journal

Relay records live under the task-session store and are append-only. Every
append validates task type, task owner, lease epoch, attempt ID, contiguous
sequence, and digest. Repeating the same `(attempt, sequence, digest)` is
idempotent; the same sequence with a different digest, a gap, a stale lease, or
an old attempt fails closed.

The relay uses bounded length-prefixed records with a checksum instead of
newline heuristics. A partial final record is ignored until complete. Corruption
before the final record quarantines that task relay and surfaces a typed health
issue; it does not block unrelated tasks.

The surface projection stores the last applied attempt, source sequence, and
digest. That cursor is the durable acknowledgement. After restart or attach,
the actor scans active tasks plus terminal tasks whose relay is not drained,
reads after the surface cursor, and resumes committing. A crash after ledger
append but before process-local acknowledgement is resolved by probing the
stable surface commit ID and then advancing from the recovered snapshot.

Relay backpressure never blocks a TUI subscriber: the existing surface
`Gap`/`SnapshotRequired` protocol handles slow clients. Relay retention is
bounded after a terminal event and a durable checkpoint proves all records were
projected. Terminal output larger than the display limit stays in the existing
task artifact/checkpoint store and is referenced, not copied into every ledger.

### 4. Task and subagent state commit atomically

`SurfaceTask` gains parent task, agent type, last activity, continuation and
checkpoint binding, and transcript availability. `SurfaceSubagent` gains task
and attempt identity, source cursor, phase, current structured tool, last
activity, and an explicit owner:

```rust
pub enum SurfaceSubagentOwner {
    Generation { parent: SurfaceOperationFence },
    DetachedTask { task: SurfaceTaskFence, launched_from: SurfaceOperationFence },
}
```

A detached child may outlive its launching generation, so its live authority
cannot be a forged generation fence. Start commits task and subagent creation in
one batch. Progress advances both the subagent cursor and task summary in one
batch. Completion commits task and subagent terminal state together. Terminal
state absorbs later progress; a stale attempt or owner is rejected.

Legacy `subagent.started/progress/completed` JSONL events, when retained for
headless presentation, are projected from committed surface events by one
bridge. They are not a second semantic journal and are deduplicated by child
identity and source sequence.

### 5. The TUI renders a projection, not a filesystem

`WorkflowPanelState` keeps canonical projected tasks plus local selection and
`expanded_task_ids`. It derives visible indented rows from `parent_task_id`.
It never synthesizes children from workflow labels or reads runtime stores.

- `Right` expands a node or selects its first child.
- `Left` collapses a node or selects its parent.
- `Enter` keeps existing actionable-approval behavior; otherwise it opens the
  selected child transcript.
- `Esc` closes the child transcript first, then the task panel.
- Panel navigation works while the parent is running as well as when idle.

The live row shows status, phase, turn, current tool, usage, and last activity.
The detail view uses a separate read-only `TranscriptState`; it never replaces
the parent transcript.

`read_task_transcript(task_id, expected_revision)` is a typed runtime-surface
query. It validates task/continuation/checkpoint binding and returns the latest
safe durable child checkpoint, transcript items, turn, usage, completion state,
and checkpoint revision. It returns typed unavailable, stale, or binding errors.
The TUI receives no continuation path and never fabricates an in-flight tail.

### 6. Permission belongs to the exact work that requested it

Every permission interaction contains one immutable owner/evidence value:

```rust
pub struct SurfacePermissionOwner {
    pub operation: SurfaceOperationFence,
    pub logical_turn: LogicalTurnId,
    pub tool_call_id: ToolCallId,
    pub task: Option<SurfaceTaskFence>,
    pub subagent_id: Option<SurfaceSubagentId>,
    pub subagent_attempt_id: Option<AgentAttemptId>,
    pub activity_sequence: Option<u64>,
    pub policy_epoch: PolicyEpoch,
    pub profile_digest: Sha256Digest,
    pub evidence: PermissionEvidence,
}
```

Evidence records the real capability/sandbox backend and enforcement state, plus
safe denial provenance. Stderr-derived guesses are never authoritative. If a
human-readable suggestion came from process output, the UI labels it as an
unverified inference and it cannot widen authority automatically.

The runtime actor owns one `apply_permission_decision` transaction. It validates
the exact owner and profile, commits a turn or session grant and interaction
resolution in one surface batch, enforces first-response-wins, updates
`SurfaceTask.pending_interaction_id`, and wakes the waiting tool only after the
commit succeeds. Session grants live in the session surface/settings state, not
in an adapter-side file write.

TUI, ACP, JSONL, and headless clients send the same typed decision with explicit
scope. The TUI offers `Allow this turn`, `Allow this session`, and `Deny`.
Permission authority is not derived from the process-local always-tool or
always-target allowlist. Stale task, attempt, activity, profile, or policy epoch
answers are rejected without granting anything.

### 7. Completion includes proof, not just a green status

Operation terminal state contains a bounded `SurfaceCompletionProof`:

```rust
pub struct SurfaceCompletionProof {
    pub outcome: CompletionProofOutcome,
    pub checks: Vec<SurfaceVerificationResult>,
    pub evidence: Vec<SurfaceEvidenceItem>,
    pub limitations: Vec<DisplayText>,
    pub generated_at: UnixMillis,
}
```

`Verified` requires at least one committed successful check relevant to the
operation. `Failed` retains failed checks. `Unverified` is explicit when no
check ran. Cancellation, timeout, indeterminate tools, lost workers, projection
degradation, and stale checkpoints appear as limitations and cannot be rendered
as verified completion. The TUI keeps a concise terminal line and an expandable
proof view with command, exit status, bounded output summary, and evidence.

### 8. Persisted session corruption is isolated and visible

Thread storage uses one bounded streaming scanner for plaintext and zstd input.
It caps encoded bytes, decoded bytes, line bytes, and records, and returns a
separate storage health report:

```rust
pub enum StoredSessionHealth {
    Healthy,
    RecoverableTail,
    Quarantined,
    InspectionLimited,
}
```

Only a true EOF-truncated final record is recoverable. A malformed middle
record, syntactically complete invalid tail, invalid typed semantic record, bad
zstd stream, or digest violation is quarantined. Oversize input is inspection
limited. Reports include a stable issue code, safe line/offset, and source
fingerprint.

Health is cached only in the rebuildable SQLite index. Logical quarantine never
moves, deletes, or rewrites source JSONL. Unreadable metadata remains visible by
a storage/catalog identity rather than a fabricated trusted thread ID. One bad
session cannot abort listing, pagination, search, or later healthy hits.

Resume, fork, and rename fail synchronously for quarantined or uninspected
sessions. Copy reference, archive, confirmed delete, and explicit export/recovery
remain available. Storage health is propagated through catalog/search DTOs,
JSONL APIs, and picker rows, and remains distinct from live
`SurfaceSessionHealth`.

### 9. First run and doctor share one literal contract

The public top-level CLI is:

```text
orca
orca doctor [--cwd PATH] [--format text|json]
orca trust [show|add|remove] [--cwd PATH]
orca exec ...
orca workflow ...
```

`orca doctor` is library-owned and read-only. It reports version, requested and
canonical cwd, config parse/source health, credential presence and source (never
the value), model/base URL presence, folder trust, and local platform sandbox
readiness. Text and stable JSON use pass/warn/fail checks with remediation.
Warnings exit zero; required failures exit one. Version one does not call the
provider or start MCP servers.

The first-run TUI uses the same home/config/auth path resolver and doctor checks.
It shows the real auth path, current workspace, trust state, and sandbox state
before the first delegated run. Trust changes and credential writes remain
explicit user actions. Unknown trust fails closed.

Public docs and tests use only `@blade-ai/orca` and `orcaagent.dev`, current
flags, current config precedence, current storage paths, and implemented
commands. Obsolete `@orcla/cli`, `orca.ai`, `orca config`, `orca sessions`, and
unsupported flags are deleted rather than aliased.

## Failure And Restart Semantics

- Child sink failure: the current child stops before acknowledging unrecorded
  activity. If a tool may already have executed, continuation state becomes
  indeterminate rather than retrying it.
- Worker crash: the relay and continuation checkpoint determine the recovery
  boundary. A safe checkpoint with no active unsafe tool may suspend/resume; an
  unknown external effect remains indeterminate.
- Actor crash: recover the surface snapshot, probe stable commit IDs, then drain
  relay records after the stored source cursor.
- Duplicate delivery: same identity/sequence/digest is a no-op. Conflicting
  digest or noncontiguous sequence is corruption and fails closed.
- Subscriber lag: existing gap/snapshot recovery provides the latest state;
  transcript details are fetched by typed query.
- Permission responder disconnect: interaction remains pending; no grant is
  written. A stale response cannot resolve a newer child activity.
- Corrupt session: isolate the entry, keep it visible, and continue catalog and
  search work for other sessions.

## Compatibility And Deletion

There is no compatibility facade for the old behavior. The implementation
deletes:

- `TuiInteractionResponse::Permission(bool)` and permission use of local
  always-tool/always-target lists;
- ACP and JSONL adapter-owned session-grant persistence;
- production child `EventSink<io::Sink>` paths that discard activity;
- TUI projection assignments that replace known child facts with `None`;
- direct TUI filesystem/continuation transcript access;
- malformed-middle-line skipping and blanket malformed-tail recovery;
- docs for nonexistent commands, old package/domain, and unsupported flags; and
- byte-compatibility fixtures that require the retired permission wire model.

New persisted schemas are explicitly versioned. Unsupported old derived index,
surface, relay, or interaction records are rebuilt where they are derived, or
fail closed with a typed unsupported-version diagnostic. Authoritative source
transcripts and user artifacts are preserved.

## Acceptance

1. Sync and detached children publish started, ordered progress, checkpoint,
   permission, and terminal facts with one task/subagent/attempt identity.
2. Killing and restarting the parent while a detached child runs replays every
   unapplied relay event exactly once and never repeats a possibly applied tool.
3. Surface start/progress/terminal batches preserve parent hierarchy, enforce
   owner/attempt/source sequence, and atomically converge task and subagent.
4. The TUI shows a navigable expandable tree while the parent runs, live tool
   activity within the throttle bound, and a checkpoint-backed child transcript.
5. Permission choices retain exact profile/scope/owner/provenance across TUI,
   ACP, JSONL, and headless surfaces; stale answers and failed commits leave no
   orphan grants.
6. Every terminal operation renders verified, failed, or explicitly unverified
   proof. Indeterminate side effects cannot appear as verified success.
7. A corrupt or oversized session remains visible and isolated; healthy sessions
   still paginate and search; source bytes remain unchanged.
8. `orca doctor` is read-only, redacts secrets, reports config/trust/sandbox
   truth, has stable text/JSON/exit behavior, and never starts the TUI as a
   prompt.
9. First run shows the resolved auth path and workspace trust/sandbox state.
   English and Chinese docs contain only the canonical package/domain and real
   CLI/config contract.
10. Focused RED/GREEN tests, cross-surface contract tests, real PTY flows,
    runtime/platform validators, full locked workspace tests, site build,
    formatter, and diff integrity pass. Independent review finds no second fact
    source, unfenced child event, orphan grant, unsafe replay, hidden corruption,
    or unsupported public claim.

## Spec Self-Review

The design keeps execution facts in the runtime actor, durable detached delivery
in a fenced relay, live presentation in the runtime surface, and local UI state
in the TUI. It distinguishes activity labels from private reasoning, relay
history from transcript checkpoints, task health from session-file health, and
completion status from verification proof. Crash windows have explicit repair
boundaries, and the no-backward-compatibility choice removes old authorities
without destroying user source data.
