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
    pub surface_commit_id: SurfaceCommitId,
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
idempotent. `surface_commit_id` is generated once with the source event, before
either direct publication or relay append, and is reused for every commit retry.
It is therefore durable on the detached path rather than reconstructed after a
crash. Tool-call identity is structured, not embedded only in display text.

The child loop emits phase labels and user-visible assistant/tool facts. It does
not emit hidden reasoning content. A `Reasoning` phase means only that the model
is working.

The thread actor is the only owner allowed to convert source activity into
`TaskPatch` and `SubagentPatch` commits. The runtime surface ledger, reducer,
projection, cursor, replay, and subscriber gap protocol remain the only live UI
fact source. `TaskRegistry` is a durable task/lease index and latest-state mirror,
not a second presentation state machine.

### 2. Sync and detached delivery use one contract

`ChildAgentActivityObserver` becomes a fallible `ChildAgentActivitySink`. The
execution admission wrapper and complete child loop together emit started,
phase/tool/usage/checkpoint progress, and one terminal event through that sink.
The wrapper owns started/terminal uniqueness; the loop owns its interior facts.

- A synchronous child uses an actor-owned ingress. Each accepted event is
  validated and committed to the runtime surface before the sink acknowledges
  it.
- A detached worker cannot own a surface commit permit. It appends the same
  event envelope to a durable relay under its current task lease. A thread-actor
  relay drainer validates the lease/attempt and commits events through the same
  ingress used by synchronous children.

The worker must not hold a parent `SessionWriter`, surface coordinator, or TUI
channel. The parent must not poll `TaskRegistry` for UI activity.

`ToolStarted` is an execution-admission fence, not a best-effort status update.
The synchronous sink must durably commit it, or the detached sink must durably
append it, before tool launch. Failure before that acknowledgement means the tool
was not admitted. Once `ToolStarted` is acknowledged, absence of a matching
terminal receipt is treated as potentially applied: the continuation becomes
indeterminate and the tool is not automatically replayed. A replay-safe tool may
be retried only through its existing explicit `ReplaySemantics` contract.

### 3. The detached relay is a delivery journal

Relay records live under the task-session store and are append-only. Every
append validates task type, task owner, lease epoch, attempt ID, contiguous
sequence, stable commit ID, and digest. Repeating the same
`(attempt, sequence, commit_id, digest)` is idempotent; the same sequence with a
different commit ID or digest, a gap, a stale lease, or an old attempt fails
closed.

The relay uses bounded length-prefixed records with a checksum instead of
newline heuristics. An encoded record is at most 64 KiB, one attempt relay is at
most 16 MiB, and a read page is at most 256 records or 1 MiB. Relay paths come
only from validated task-store identities, stay below the canonical task-session
root, and use no-follow/exclusive create and append semantics. No user text is
interpolated into a path. A partial final record is ignored until complete.
Corruption before the final record quarantines that task relay and surfaces a
typed health issue; it does not block unrelated tasks.

A live reader stops at an incomplete final record. A newly acquired lease first
proves the previous writer is fenced, then truncates only that incomplete tail to
the last checksummed boundary under the task lock before appending. It never
repairs a complete-but-invalid record; that relay is quarantined. This prevents a
dead writer's tail from blocking a valid takeover.

The surface projection stores the last applied attempt, source sequence, commit
ID, and digest in the same surface commit as the projected activity. That cursor
is the durable acknowledgement. After restart or attach,
the actor scans active tasks plus terminal tasks whose relay is not drained,
reads after the surface cursor, and resumes committing. A crash after ledger
append but before process-local acknowledgement is resolved by probing the
stable surface commit ID and then advancing from the recovered snapshot.

Relay backpressure never blocks a TUI subscriber: the existing surface
`Gap`/`SnapshotRequired` protocol handles slow clients. After the terminal source
cursor and its checkpoint are durably present in the surface, the delivery
journal may be compacted to a small tombstone containing attempt, terminal
sequence, commit ID, and digest. Terminal output larger than the display limit
stays in the existing task artifact/checkpoint store and is referenced, not
copied into every ledger.

The multi-store authority order is explicit: continuation state owns
external-effect recovery; the relay is an at-least-once delivery journal; the
surface ledger owns user-visible activity, interactions, and grants; and
TaskRegistry is a repairable latest-state mirror. Startup reconciliation handles
launch failure after committed `Started`, checkpoint-before-notification,
terminal-relay-before-continuation-terminal, continuation-terminal-before-task
mirror, surface-commit-before-local-ack, and worker death between acknowledged
`ToolStarted` and `ToolCompleted`. No mirror or delivery record can override a
newer continuation or surface authority.

### 4. Task and subagent state commit atomically

`SurfaceTask` gains parent task, agent type, continuation and checkpoint
binding, and transcript availability. `SurfaceSubagent` gains task and attempt
identity, source cursor, phase, current structured tool, and an explicit owner.
The source cursor carries the event's `occurred_at`; task summaries derive last
activity from that authoritative child cursor instead of task start/completion:

```rust
pub enum SurfaceSubagentOwner {
    Generation { parent: SurfaceOperationFence },
    DetachedTask {
        task: SurfaceTaskOwnerRef,
        launched_from: SurfaceOperationFence,
    },
}

pub struct SurfaceTaskOwnerRef {
    pub task_id: SurfaceTaskId,
    pub task_revision: TaskRevision,
    pub attempt_id: AgentAttemptId,
    pub authority_digest: Sha256Digest,
}
```

A detached child may outlive its launching generation, so its live authority
cannot be a forged generation fence. `SurfaceTaskOwnerRef` is public identity,
not authority: the actor retains the non-serializable `SurfaceTaskFence` and
validates the reference/digest before issuing a commit permit. Private background
owner tokens never enter a snapshot, relay, diagnostic, JSONL event, ACP message,
or TUI value.

The actor creates and commits `Started` before launching either child mode. A
detached worker receives the next sequence and stable event/commit seed. Launch
failure therefore has a visible failed terminal child without a hidden process.
Progress advances both the subagent cursor and task summary in one batch.
Completion commits task and subagent terminal state together. Terminal state
absorbs later progress; a stale attempt or owner is rejected.

Parent task identity is same-thread and acyclic. The reducer rejects self-parent,
missing parent, cross-thread parent, cycles, and depth greater than 32 before a
commit. A TUI projection that somehow violates these invariants is rejected as a
degraded snapshot; the panel retains its last accepted tree rather than inventing
an orphan root.

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

The default conversation view shows a bounded stack of the most recently active
ordinary subagents, including status, turn, current activity, usage, and elapsed
time. Overflow is explicit rather than silently replacing per-agent activity
with only a total count. The Agents panel combines ordinary subagents and
workflow agents under one parent/agent/status/detail projection. The Tasks panel
remains the complete tree and transcript entry point. Its detail view uses a
separate read-only `TranscriptState`; it never replaces the parent transcript.

`read_task_transcript(task_id, expected_revision)` is a typed runtime-surface
query. It validates task/continuation/checkpoint binding and returns the latest
safe durable child checkpoint, transcript items, turn, usage, completion state,
and checkpoint revision. It returns typed unavailable, stale, or binding errors.
The TUI receives no continuation path and never fabricates an in-flight tail.

### 6. Permission belongs to the exact work that requested it

Every permission interaction contains one immutable public owner and evidence
value:

```rust
pub enum SurfacePermissionOwner {
    Foreground {
        operation: SurfaceOperationFence,
        logical_turn: LogicalTurnId,
        tool_call_id: ToolCallId,
    },
    Detached {
        task: SurfaceTaskOwnerRef,
        child_turn: LogicalTurnId,
        activity_sequence: u64,
        tool_call_id: ToolCallId,
    },
}

pub struct SurfacePermissionRequestIdentity {
    pub owner: SurfacePermissionOwner,
    pub request_policy_epoch: PolicyEpoch,
    pub profile_digest: Sha256Digest,
    pub evidence: PermissionRequestEvidence,
}
```

`PermissionRequestEvidence` distinguishes a prospective
`PreflightEnforcementAssessment`, an authoritative `ObservedDenialReceipt`, and
an `UnverifiedInference`. A pre-launch prompt cannot claim an observed backend
receipt. Stderr-derived guesses are always unverified, are labeled that way, and
cannot widen authority automatically.

The runtime actor owns request and decision batches. A request batch atomically
creates the interaction and sets the task's singular
`pending_interaction_id`; one child task can have only one pending permission.
A decision batch validates every precondition against the pre-state, resolves
with the request policy epoch, optionally commits a Session grant to a new policy
epoch, clears the pending task interaction, and records both request and resulting
epochs in the grant receipt. A dedicated commit authority validates this batch as
one transition rather than relying on sequential reducer event order. The
surface ledger is authoritative; in-memory config, metadata, and TaskRegistry are
repairable projections.

A detached worker exchanges permission requests and committed decisions through
a lease-fenced bidirectional task mailbox. Worker-to-actor requests are relay
records and are projected as the request batch. After a user decision batch is
durable, the actor appends a decision-delivery receipt keyed by interaction ID
and digest. If delivery append fails or the actor crashes, reconciliation derives
the missing delivery from the surface ledger. The worker validates the task,
attempt, interaction, policy epochs, profile, and digest before installing it;
the mailbox never grants authority on its own. Until receipt delivery, the worker
waits without launching the tool.

A Turn grant is bound to one child attempt and child logical turn and expires at
that turn boundary. A Session grant belongs to the owning thread session,
survives runtime restart through surface settings, reaches the current worker in
its decision receipt, and is hydrated by later parent and child turns. `Deny` is
interaction-local and has no grant scope. ACP `reject_always` is removed rather
than pretending to persist a deny rule.

TUI, ACP, JSONL, and headless clients send the same typed decision; allow carries
an explicit scope and deny does not. The TUI offers `Allow this turn`,
`Allow this session`, and `Deny`.
Permission authority is not derived from the process-local always-tool or
always-target allowlist. Stale task, attempt, activity, profile, or policy epoch
answers are rejected without granting anything.

Tool approval remains a separate typed interaction with its own recovery
contract. `TaskRegistry.pending_tool_approval_response: Option<bool>` may not be
used as permission authority or a presentation fallback, and all direct
TaskRegistry permission projection/resolution paths are deleted.

### 7. Completion includes proof, not just a green status

`OperationTerminalRecord` contains a bounded
`SurfaceOperationCompletionProof`:

```rust
pub struct SurfaceOperationCompletionProof {
    pub verification: Option<SurfaceVerificationResult>,
    pub tool_receipts: Vec<SurfaceToolCompletionReceipt>,
    pub resume: Option<SurfaceResumeBoundary>,
    pub limitations: Vec<DisplayText>,
}
```

Tool receipts retain tool-call ID, typed terminal, canonical result digest, and
optional file path/change digest; large output and diffs remain in their existing
tool projections. Usage remains in `OperationTerminalRecord.usage`. The outer
operation record and `OperationTerminalAtCursor` remain the durable identity and
commit witness, so proof does not duplicate those fields.

The displayed outcome is derived: verified requires a committed successful
verification result and no indeterminate receipt; failed retains a failed
verification; unverified is explicit when no verification ran. Cancellation,
timeout, indeterminate tools, lost workers, projection degradation, and stale
checkpoints appear as limitations and cannot be rendered as verified completion.
The TUI keeps a concise terminal line and an expandable proof view with command,
exit status, bounded output summary, tool receipts, and resume boundary.

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

A complete parseable final record without a trailing newline is healthy. A
lexically partial JSON record at plaintext EOF is a recoverable ignored tail. A
malformed middle record, syntactically complete invalid final record, invalid
typed semantic record, bad zstd stream, decompression truncation, or digest
violation is quarantined; compressed corruption is never guessed to be a tail.
Oversize input is inspection limited. Reports include a stable issue code, safe
line/offset, and source fingerprint.

Health is cached only in the rebuildable SQLite index. Logical quarantine never
moves, deletes, or rewrites source JSONL. Unreadable metadata remains visible by
a storage/catalog identity rather than a fabricated trusted thread ID. One bad
session cannot abort listing, pagination, search, or later healthy hits.

Resume, fork, and rename fail synchronously for quarantined or
inspection-limited sessions. A recoverable tail resumes only from the previous
fully committed boundary and displays a warning. Copy reference, archive,
confirmed delete, and explicit bounded raw export/recovery remain available for
quarantined and inspection-limited entries. `thread/read` returns a stable typed
health error unless explicit recovery/export was requested. Storage health is
propagated through catalog/search DTOs, JSONL APIs, and picker rows, and remains
distinct from live `SurfaceSessionHealth`.

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
This gate is keyed to the canonical workspace, onboarding schema version, and
security-policy digest, not merely to credential absence. A small onboarding
acknowledgement under `ORCA_HOME` records that the facts were shown; an env or
auth-file key cannot bypass it, and a policy change reopens it. The screen shows
the real auth path, current workspace, trust state, and sandbox state before the
first delegated run. It may acknowledge running with fail-closed untrusted
policy, but only explicit `orca trust add` mutates trust. Credential writes are
also explicit.

Public docs and tests use only `@blade-ai/orca` and `orcaagent.dev`, current
flags, current config precedence, current storage paths, and implemented
commands. A checked public CLI manifest is mechanically compared with Clap's
command tree, and the EN/ZH docs validator admits only that manifest plus prose.
Obsolete `@orcla/cli`, `orca.ai`, `orca config`, `orca sessions`, `orca goal`,
`orca context`, `--max-cost`, `--output`, `--jsonl`, and unsupported approval or
provider-health claims are deleted rather than aliased.

## Failure And Restart Semantics

- Child sink failure: before `ToolStarted` acknowledgement, no tool is admitted.
  After that acknowledgement and before a matching durable terminal receipt, the
  tool is potentially applied and continuation state becomes indeterminate
  rather than retrying it.
- Worker crash: the relay and continuation checkpoint determine the recovery
  boundary. A safe checkpoint with no active unsafe tool may suspend/resume; an
  unknown external effect remains indeterminate.
- Actor crash: recover the surface snapshot, probe stable commit IDs, then drain
  relay records after the stored source cursor.
- Duplicate delivery: delivery is at least once; the same
  identity/sequence/commit/digest reduces to one durable projection. Conflicting
  identity material or noncontiguous sequence is corruption and fails closed.
- Subscriber lag: existing gap/snapshot recovery provides the latest state;
  transcript details are fetched by typed query.
- Permission responder disconnect: interaction remains pending; no grant is
  written. A crash after decision commit but before worker delivery is repaired
  from the surface ledger. A stale response cannot resolve a newer child
  activity.
- Corrupt session: isolate the entry, keep it visible, and continue catalog and
  search work for other sessions.

## Compatibility And Deletion

There is no compatibility facade for the old behavior. The implementation
deletes:

- `TuiInteractionResponse::Permission(bool)` and permission use of local
  always-tool/always-target lists;
- ACP and JSONL adapter-owned session-grant persistence;
- permission use of TaskRegistry bool approval responses and direct registry
  presentation fallbacks;
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
2. Killing and restarting the parent while a detached child runs may redeliver
   relay events, but each reduces to one durable projection and no possibly
   applied tool is automatically repeated.
3. Surface start/progress/terminal batches preserve parent hierarchy, enforce
   owner/attempt/source sequence, and atomically converge task and subagent.
4. The TUI shows active children up to the bounded default live-stack capacity,
   reports overflow explicitly, includes ordinary and workflow children in the
   Agents panel, and keeps a navigable expandable task tree while the parent
   runs. Tool boundaries are unthrottled, streaming activity is capped at four
   source events per second, committed activity is visible within one second in
   the deterministic PTY test, and child transcripts are checkpoint-backed.
5. Permission choices retain exact profile/scope/owner/provenance across TUI,
   ACP, JSONL, and headless surfaces; detached request/decision delivery survives
   restart; stale answers and failed commits leave no orphan grants.
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
11. Fault injection covers parent and worker death at every tool boundary,
    permission request/decision commit ambiguity, settings append/checkpoint/
    projection failure, responder races, Turn expiry, Session grant restart,
    incomplete relay-tail takeover, and old-schema fail-closed visibility/export.

## Spec Self-Review

The design keeps execution facts in the runtime actor, durable detached delivery
in a fenced relay, live presentation in the runtime surface, and local UI state
in the TUI. It distinguishes activity labels from private reasoning, relay
history from transcript checkpoints, task health from session-file health, and
completion status from verification proof. Crash windows have explicit repair
boundaries, and the no-backward-compatibility choice removes old authorities
without destroying user source data.
