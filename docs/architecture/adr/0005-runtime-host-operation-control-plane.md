# ADR 0005: Runtime Host Operation Control Plane

- Status: Accepted; P0.3a through P0.3e4c implemented, final release validation pending
- Date: 2026-07-15
- Roadmap: P0.3 Runtime Operation Host and canonical turn executor

## Context

At the start of P0.3, Orca had improved cancellation and worker cleanup, but
still had multiple owners for one conceptual agent operation.

- `orca-tui::agent_runner` owned a TUI-only provider, tool, compaction, hook,
  and turn loop plus an outer agent thread and provider workers.
- `RuntimeThread` exposed borrowed synchronous mutation and delegated each
  request to `ThreadTurnExecutor`.
- `run_thread_turn_inner_with_events` had separate branches for caller-owned
  and internally-created `EventFactory` values, duplicating turn assembly.
- The server moved a whole `ServerThread` into a per-turn OS thread while
  `ActiveTurnManager` separately owns generation, cancellation, resume, join,
  and reclamation state.
- `orca-runtime/src/lib.rs` contained more than one thousand
  `include_str!`/`contains` source-shape checks. These tests can preserve
  obsolete names and file layouts without proving cancellation, joining, or
  terminal delivery.

The result was an architecture defect, not a local cancellation defect. There
was no shared runtime handle that could prove all of the following for TUI,
server, and headless callers:

1. exactly one owner admits commands for a thread;
2. every operation has a fresh cancellation scope;
3. stale commands cannot affect a newer operation;
4. completion is available independently of event subscribers;
5. shutdown cancels, waits for, and reclaims active work;
6. thread state is owned either by the actor or by its one active task, never
   borrowed by an external surface during a turn.

Current Codex main reinforces this ownership model: one session submission
loop owns thread commands, a running turn owns its cancellation token and task
handle, completion has an independent notification, and `shutdown_and_wait`
waits for the session loop. Codex steering also fences input by the expected
turn id. Claude Code `2.1.210` no longer ships inspectable implementation
source in its npm package, so current internal ownership cannot be independently
confirmed there. The local restored `2.1.88` source used one conversation-owned
`QueryEngine` and explicit abort control. Orca adopts the evidence-backed
ownership properties, not either implementation.

## Decision

Introduce the P0.3 control plane in `orca-runtime`:

### RuntimeHost

`RuntimeHost` owns one process Tokio runtime and a supervisor task. Dropping or
shutting down the host sends a typed shutdown command and joins the supervisor
thread. The host never abandons actors or operation tasks.

The host command mailbox is bounded. Thread creation returns a cloneable
`RuntimeThreadHandle`; callers do not receive mutable access to the actor-owned
`RuntimeThread`.

### ThreadActor

One `ThreadActor` owns one conversation and serializes a bounded typed command
mailbox. P0.3a accepts:

- `StartTurn`;
- `InterruptOperation`;
- `ReadState`;
- `ShutdownThread`.

The actor is the sole authority for the current logical operation id. It
rejects a second start while an operation is active. P0.3c extends the same
mailbox with actor-owned generation, resume, steer, and generation-admission
commands instead of creating another surface control plane.

`StartTurn` carries an owned `HostedTurnRequest`. It accepts only thread-safe,
owned interaction and event handlers. The legacy `ThreadTurnRequest` can still
carry borrowed TUI handlers, so it is constructed only inside the operation
task and is never falsely marked or treated as `Send`.

When idle, the actor owns `RuntimeThread`. While a turn runs, ownership moves
into one joined operation task. The actor then owns the task handle, fresh
cancel scope, operation id, and terminal completion. When the task finishes,
the thread returns to the actor before another turn can start.

### OperationHandle And Terminal Completion

Starting a turn returns an `OperationHandle` containing the thread id,
operation id, command authority, and a shared one-shot completion cell. The
completion cell records one typed terminal outcome and supports waiting without
reading runtime events.

Dropping an `OperationHandle` or an event receiver does not cancel the
operation. Cancellation requires an explicit typed interrupt command.

Terminal outcomes distinguish at least:

- completed with `RunStatus`;
- execution failure;
- operation task panic or join failure.

An interrupt request is only an acknowledgement that the actor cancelled the
matching scope. The terminal outcome is published after the operation task has
actually stopped and returned its owned thread state.

### Canonical Turn Kernel And Background Handoff

`ThreadTurnExecutor` is the only provider, tool, compaction, hook, persistence,
usage, and terminal kernel used by TUI, server, and headless operations.
`HostedGenerationHandlers` carries generation-fenced approval, permission,
user-input, MCP elicitation, and provider-suspension control into that kernel.

The kernel returns a typed completed-or-provider-suspended outcome plus every
non-waiting workflow handle it launched. `ThreadActor` returns its
`RuntimeThread` to idle ownership before handing provider or workflow work to
the host's bounded background registry. That registry owns cancellation, join,
task settlement, usage, continuation state, and terminal event publication.
No surface-specific executor assembles a second agent loop.

## User Value

This slice establishes the behavior needed for reliable TUI task control:

- an interrupt can target exactly the operation shown as running;
- returning control to the UI cannot precede task cleanup and join;
- renderer or event-subscriber loss cannot silently cancel work or lose the
  authoritative terminal state;
- thread shutdown has a joined, testable path instead of relying on detached
  worker lifetime;
- the next TUI migration can use a runtime handle without preserving the TUI's
  current outer cancellation owner.

P0.3a is a foundation slice and is not a release point by itself. A release
requires a migrated surface with a user-visible reliability improvement.

## Ownership Model

At every instant, ownership is one of these states:

```text
RuntimeHost supervisor
  -> ThreadActor
       -> idle: RuntimeThread
       -> running: ActiveOperation
            -> terminal completion cell
            -> actor-owned request and steer queue
            -> ActiveGeneration
                 -> GenerationFence
                 -> fresh cancel token
                 -> joined task handle
                 -> RuntimeThread + EventFactory + writer (inside task)
       -> background registry (bounded)
            -> provider suspension + cancel + joined task
            -> workflow handle + cancel + joined task
            -> task/history/usage/continuation settlement
```

No external caller owns `RuntimeThread`, the operation join handle, or the
cancel token. Handles carry command authority only. TUI task controls address
the same host-owned task registry and continuation path used by canonical turn
execution.

## External Compatibility

P0.3a does not change:

- CLI arguments or exit codes;
- TUI key bindings, transcript behavior, or permission flow;
- server JSONL request or event shapes;
- persisted thread/session formats;
- provider retry, streaming, compaction, or tool semantics.

TUI, server, and headless now use the same host and canonical turn behavior.
The migration added only internal typed host commands and additive runtime
events; it did not require a wire or persistence migration.

## Migration Sequence

1. Add the host, actor, typed commands, operation handle, and terminal cell
   behind behavior tests.
2. Move headless session and event ownership into the actor.
3. Add actor-owned logical-turn generations and typed resume, steer, and
   generation-fenced input admission.
4. Migrate server active turns to `RuntimeThreadHandle`; delete server-owned
   generation, cancellation, resume mailbox, and reaper state.
5. Migrate the TUI session and operation controller to `RuntimeHost`, then
   replace its provider/tool loop with the canonical `ThreadTurnExecutor`.
6. Move provider suspension and non-waiting workflow ownership into the host's
   bounded background registry.
7. Delete the TUI executor, provider/tool/workflow loop, task supervisor,
   provider facade, detached workflow watcher, and their source-shape tests.

All seven steps are implemented in P0.3a through P0.3e4c. Release validation
and main-worktree integration remain separate from this architecture decision.

## P0.3b: Actor-Owned Session And Event Lifecycle

### Structural Problem And Evidence

P0.3a deliberately left event and session ownership outside the host. The
legacy host executor calls `RuntimeThread::run_request_with_cancel`, which
creates a new `EventFactory` for each operation. The headless controller owns a
second lifecycle envelope around that path: it constructs `RuntimeThread` and
`EventFactory`, emits `session.started`, runs `SessionStart`, suppresses the
turn-level terminal, runs `SessionEnd`, and emits `session.completed`.

Moving headless onto P0.3a unchanged would therefore either reset event
sequence numbers inside the actor or preserve a controller-owned outer session
around a host-owned turn. Both outcomes leave two lifecycle facts and make the
first production migration untrustworthy.

### Target Ownership And Module Boundary

`ThreadActor` owns one idle state bundle containing both `RuntimeThread` and a
persistent `EventFactory`. The same bundle moves into the one joined operation
task and returns to the actor before another operation is admitted. The
operation executor receives the actor-owned factory together with the cancel
scope; it may emit turn events but may not create a replacement factory.

`HostedTurnRequest` carries a typed operation envelope:

- `Turn` preserves the existing turn-level completion option for legacy
  callers while they migrate;
- `HeadlessSession` makes the host the sole owner of `session.started`,
  `SessionStart`, turn execution, `SessionEnd`, and the final
  `session.completed` event.

The headless controller becomes a synchronous client of `RuntimeHost`. Its
public writer APIs continue to accept borrowed writers. A bounded,
acknowledged event relay bridges the host-owned operation task to that writer:
each flushed event is accepted only after the caller writer succeeds, and a
downstream write error is returned through the relay so the operation records
typed execution failure rather than reporting cancellation.

The host exposes immutable thread-start diagnostics needed by the controller,
but never mutable `RuntimeThread`, `InteractiveSession`, event-factory, cancel,
or join ownership.

### TUI User Value

Headless is the smallest production path that can prove the runtime host owns a
complete operation rather than only wrapping an internal function. This
removes the event-sequence and session-envelope ambiguity that would otherwise
be carried into the TUI migration. It directly lowers the risk that a later TUI
interrupt returns before cleanup, that renderer replacement resets event
identity, or that terminal state differs between the runtime and the surface.

### External Compatibility

P0.3b keeps CLI arguments, exit codes, text output, JSONL event names and
payloads, event ordering, persisted session format, provider behavior, and
desktop-notification behavior compatible. `run_to_writer` and
`run_to_writer_with_options` retain borrowed-writer support. Server and TUI
execution ownership do not migrate in this slice.

### Completed Migration Order

1. Add behavior tests for persistent actor event sequence, host-owned session
   ordering, typed writer failure, and shutdown/join behavior.
2. Move `EventFactory` into the actor state bundle and pass it through the
   operation executor with the operation cancel token.
3. Add the typed headless session envelope and bounded acknowledged writer
   relay.
4. Replace `controller::run_inner` with host startup, one hosted headless
   operation, relay draining, terminal mapping, and host shutdown.
5. Delete direct headless `RuntimeThread`, `EventFactory`, hook, and session
   terminal ownership in the controller.

The server and TUI remain temporary legacy surface owners until their own
vertical migrations. They may use the actor-owned event sequence later, but
must not add another session envelope around it.

## P0.3c: Actor-Owned Logical Turn Generations And Input Admission

### Structural Problem And Evidence

P0.3b made one host operation joinable, but it still treats every executor run
as the complete operation. That boundary cannot replace the production server
control plane:

- `server::ActiveTurnControl` owns a resettable generation record containing
  the generation id, cancel token, and command-admission flag;
- `run_thread_submit_async` owns a second loop that waits for one generation,
  checks a separate resume mailbox, creates the next cancel token, and reruns
  the same persisted turn;
- `ActiveTurnManager` separately owns the worker join handle, reclamation,
  shutdown reaper, generation validation, steer queue, and session permission
  metadata;
- permission, user-input, and MCP replies ask that manager whether their
  captured generation is still active, while turn steer bypasses the host and
  pushes directly into a shared `ThreadSteerHandle`;
- the server writer buffers terminal-looking JSONL lines so an interrupted
  generation cannot publish the logical turn terminal before a queued resume.

Migrating the server before moving those responsibilities would leave
`ActiveTurnManager` around `RuntimeHost` as a permanent second control plane.
The actor would own an operation task while the server still decided which
generation is current, whether input is accepted, and when the turn is really
terminal. That contradicts the P0 ownership model.

Current Codex keeps expected-turn validation inside the session that owns the
active task. Its app-server maps `expectedTurnId` into
`Session::steer_input`, which atomically checks the active task and turn id
before queuing input. Orca should preserve that ownership property while also
keeping its explicit interrupted-generation resume semantics.

### Target Ownership And Module Boundary

One `OperationId` identifies the actor-owned logical turn for its complete
lifetime. Each executor attempt has an opaque monotonically increasing
`GenerationId`, starting at zero, and a typed `GenerationFence` containing both
ids.

`ThreadActor` exclusively owns:

- logical-turn admission and the one authoritative `OperationCompletion`;
- current generation id, fresh cancel token, and joined task handle;
- interrupt and resume admission, including duplicate-resume coalescing;
- the steer queue and the rule for when same-turn input is accepted;
- validation of generation-scoped permission, user-input, and MCP replies;
- the decision to start a replacement generation only after the previous task
  has joined and returned thread, event, request, and writer ownership.

The actor command mailbox gains typed operations for resume, steer, generation
validation, state reads, and shutdown. Every result identifies the logical
operation and, where relevant, the generation it observed. A stale logical
operation id or generation fence can never affect the current generation.

Interrupt, resume, and steer are logical-turn commands: an accepted command
intentionally targets whichever generation is current for the matching
`OperationId`. Permission, user-input, and MCP replies are generation-scoped
and must present the exact `GenerationFence` captured when the request was
created. This distinction lets a user keep controlling one resumed turn while
preventing an answer produced for generation N from entering generation N+1.

The executor receives its `GenerationFence` and an actor-created steer handle.
`HostedTurnRequest` no longer accepts an externally supplied steer handle that
could bypass actor admission. A resumed generation runs the same owned request
with the existing-turn marker set, so the original user prompt is not appended
again.

An interrupt cancels only the current generation. Resume is admissible only
after that interrupt and is queued on the logical turn; the replacement
generation is not spawned until the cancelled generation has joined. The
logical operation completion remains empty across that transition and is
written exactly once after the final generation joins.

### TUI User Value

This slice removes the lifecycle race that would otherwise reach the TUI when
pause/resume and same-turn input move onto the host:

- Resume cannot start a new provider/tool loop while the previous one still
  owns the conversation, writer, or resources.
- A stale turn control cannot mutate a newer logical operation, and a delayed
  permission, user-input, or MCP answer cannot mutate a newer generation of
  the same turn.
- The UI can distinguish "interrupt accepted", "resume queued", and "turn
  terminal" instead of treating cancellation acknowledgement as cleanup.
- The eventual TUI migration can delete its outer cancellation owner rather
  than wrapping the actor in another resettable scope.

P0.3c is still a foundation slice and is not a release point. Its concrete
product value is eliminating the resume/input race before the server and TUI
adopt the host.

### External Compatibility

P0.3c does not change CLI arguments, TUI interaction flow, server JSONL request
or event shapes, persisted session format, or provider/tool behavior. Headless
sessions remain single-generation and explicitly reject resume while retaining
the P0.3b session envelope.

The new host command/result types are runtime-internal migration APIs. Existing
headless callers keep `start_turn`, interrupt, wait, and shutdown behavior.

### Migration Order And Temporary State

1. Add RED runtime-host behavior tests for generation identity, join-before-
   resume, duplicate resume, stale fences, steer admission, prompt reuse, one
   logical terminal, and shutdown.
2. Add typed generation ids, fences, command results, and state snapshots.
3. Move the steer queue into `ThreadActor` and remove the external host-request
   steer-handle escape hatch.
4. Return owned thread/event/request/writer state from each generation task and
   let the actor either start the joined replacement or publish the final
   logical terminal.
5. Keep server production execution on its legacy control plane until its
   adapter can be replaced vertically in the next slice.

The temporary boundary is explicit: P0.3c makes the host the complete source of
truth only for host-run logical turns. The legacy server still owns its old
turns until migration, and its JSONL terminal buffering remains there only
until the server adapter consumes actor generation outcomes. No new server or
TUI lifecycle state may be added beside it.

### P0.3c Acceptance Criteria

1. One logical operation id remains stable while generation ids increase from
   zero and every generation receives a fresh cancellation token.
2. Resume is rejected before interrupt, duplicate accepted resumes coalesce,
   and generation N+1 cannot enter the executor until generation N has exited
   and joined.
3. A generation fence is accepted only for the current, command-accepting
   generation; cancellation, replacement, completion, and a newer operation
   make the old fence stale.
4. Steer input is accepted only for the matching active logical turn before
   cancellation, reaches the actor-owned queue once, and cannot be injected by
   retaining an external handle.
5. A resumed generation is marked as the existing turn and does not append or
   execute a duplicate initial user prompt.
6. Intermediate cancelled generations never complete the logical operation;
   exactly one authoritative `OperationTerminal` is published after the final
   generation joins.
7. Thread or host shutdown cancels and joins the current generation, ignores a
   queued resume, and publishes one terminal before returning.
8. Headless session ordering, event sequence, hooks, writer-failure behavior,
   and existing P0.3a/P0.3b ownership tests remain compatible.
9. Focused runtime-host and cancellation tests pass, followed by the shared
   runtime full gate and workspace Clippy with no new warnings.

### P0.3c Deletion Gate

This slice is incomplete if `ThreadActor` delegates generation replacement to
an external loop, exposes its cancel token or steer queue for direct mutation,
or completes the logical operation before the final joined generation. The
server `ActiveTurnManager`, generation writer, and legacy worker loop are not
deleted in P0.3c because the production server does not migrate in this slice;
their deletion is the mandatory completion gate of the immediately following
server migration.

### P0.3c Verification

- Seventeen runtime-host behavior tests cover logical operation and generation
  identity, fresh cancellation scopes, join-before-resume, duplicate resume,
  stale generation rejection, actor-owned steer admission, task lifecycle
  reopening, one logical terminal, headless resume rejection, and shutdown.
- A focused controller behavior test proves an existing-turn generation does
  not append the original user prompt again.
- `cargo test -p orca-runtime --all-targets -- --test-threads=1` passes with
  780 runtime unit tests, 17 runtime-host tests, and 12 task-output tests.
- `cargo test --workspace --all-targets -- --test-threads=1` passes, including
  130 server contracts and 467 TUI tests.
- `cargo clippy --workspace --all-targets` passes with the repository's
  existing warnings and no warning in the P0.3c implementation or tests.
- The real DeepSeek release harness passes provider summary, headless CLI,
  malformed-history resume and repair, server, server thread memory, active
  turn resume, turn controls, metadata, list/search, and paginated read gates.

## P0.3d: Server Active Turns On The Runtime Host

### Structural Problem And Evidence

P0.3c gives `ThreadActor` the complete logical-turn generation state machine,
but the production server still runs the same responsibilities a second time:

- `run_thread_submit_async` removes `ServerThread` from the idle registry,
  spawns a detached OS worker, and owns a loop that creates handlers, cancel
  tokens, writers, and replacement generations;
- `ActiveTurnControl` owns another generation id, resettable cancel token,
  command-admission bit, steer handle, and resume mailbox;
- `ActiveTurnManager` owns the worker join handle, polling reclamation, bounded
  shutdown waits, metadata handoff, and an `ActiveTurnReaper` for work that
  outlives server shutdown;
- permission, user-input, and MCP replies validate a raw `u64` generation
  against that manager instead of presenting the actor's `GenerationFence`;
- `GenerationServerRequestWriter` decides outside the actor whether a
  terminal-looking JSONL line belongs to a replaced or final generation;
- `ServerThreadRuntime` must take the whole mutable thread out of its map while
  a turn runs, so live thread ownership, active-turn routing, projection reads,
  and metadata updates have different sources of truth.

Wrapping this manager around `RuntimeThreadHandle` would leave the server in
charge of the exact lifecycle that P0.3 moved into the actor. The migration is
complete only when the actor permanently owns each server `RuntimeThread` and
the old worker loop and manager are deleted.

### Target Ownership And Module Boundary

`ServerThreadRuntime` owns one process-level `RuntimeHost` plus a registry of
server thread records. Each record contains a cloneable `RuntimeThreadHandle`
and server metadata such as title, cwd, workspace roots, permission profile,
additional directories, network grants, task registry, and MCP registry. It
never contains `RuntimeThread` itself.

`ThreadActor` permanently owns the live `RuntimeThread`, including while idle.
It additionally owns:

- a typed idle snapshot command for conversation projection and next-turn id;
- the effective `RunConfig` and persisted task id for each logical turn;
- creation of fresh generation-scoped interaction handlers from the current
  `GenerationFence` and cancel token;
- the output lifecycle callback that commits terminal protocol lines only for
  the final generation and drops replaced-generation terminals;
- shutdown cancellation and joining for every active server generation.

The server may retain a small `turn_id -> ServerActiveTurn` routing index. Each
entry contains only the thread id and actor `OperationHandle`. It may route
interrupt, resume, and steer commands and inspect `OperationCompletion`; it
must not own a cancel token, generation counter, steer queue, resume mailbox,
worker/reaper, or returned thread state.

Pending permission, user-input, and MCP records store the actor-issued
`GenerationFence`. Response processors ask the operation handle to admit that
exact fence before delivering the response. Session permission metadata is
updated directly in the persistent server thread record, which remains present
while a turn runs.

### TUI User Value

The server is the first production surface to exercise actor-owned resume and
input admission. This removes races before the TUI adopts the same host:

- an interrupted generation cannot overlap its replacement or leak a stale
  terminal event;
- server EOF and shutdown cancel and join the same task the control commands
  address, with no detached reaper continuing after ownership is handed off;
- permission and user-input answers cannot enter a resumed generation through
  a raw integer comparison in another manager;
- thread metadata and control remain available while the actor owns the live
  conversation, eliminating the take/put gap that the TUI would otherwise
  inherit.

P0.3d is still unreleased foundation work. It proves the production adapter and
deletes the duplicate server control plane so the next TUI slice can reuse an
already exercised lifecycle instead of being the first adopter.

### External Compatibility

P0.3d preserves CLI arguments, server JSONL request and event shapes, persisted
session format, turn ids, permission request ids, active-turn interrupt/resume/
steer behavior, thread projections, and DeepSeek provider behavior. The
server's internal generation identity changes from a raw `u64` to
`GenerationFence`, but the numeric generation component in request ids remains
stable.

Thread start, resume, fork, metadata updates, list/search, turns/items
pagination, mention search, command execution, and session permission grants
must keep their current observable behavior.

### Migration Order And Temporary State

1. Add RED host tests for per-turn config, persisted task ids, idle snapshots,
   fresh generation interaction factories, and actor-finalized generation
   output.
2. Extend typed host commands and request/output abstractions without changing
   existing headless callers.
3. Change `ServerThreadRuntime` into a process-host plus handle/metadata
   registry; migrate synchronous server-runtime behavior tests to that path.
4. Replace `run_thread_submit_async` with one actor operation and a routing
   index; route controls and generation admission through `OperationHandle`.
5. Move server terminal-line suppression under the actor-owned output
   lifecycle and create interaction handlers from the actor generation fence.
6. Delete `ActiveTurnManager`, `ActiveTurnControl`, the worker generation loop,
   resume mailbox, resettable cancellation record, polling reclamation,
   `ActiveTurnReaper`, take/put thread ownership, and the old generation writer.
7. Run focused host/server tests, the full serial workspace gate, workspace
   Clippy, and the real DeepSeek server resume/control harness.

The temporary overlap existed only while tests moved from the old server path
to the host. The completed slice retains one production active-turn control
plane: the runtime host actor.

### P0.3d Acceptance Criteria

1. Every server thread is created, resumed, or forked inside one process-owned
   `RuntimeHost`; the server registry never owns a live `RuntimeThread`.
2. Server turn start provides the persisted turn id and effective per-turn
   config to the actor, and resumed generations reopen that same task id.
3. Each generation creates permission, user-input, and MCP handlers from its
   typed `GenerationFence` and fresh cancel token; stale replies are rejected
   by actor admission.
4. Replaced generations drop cancellation errors and terminal protocol lines,
   while the final generation commits exactly one externally visible turn
   terminal under actor control.
5. Interrupt, duplicate interrupt, resume, duplicate resume, steer, stale turn
   controls, and completed-turn errors preserve the server JSONL contract.
6. Live metadata, task registry, MCP mention search, projections, next-turn id,
   command execution policy, and session grants remain available without
   taking the thread out of the registry.
7. Server EOF and explicit shutdown cancel and join all active actor
   generations before returning; no detached worker or reaper remains.
8. `ActiveTurnManager`, `ActiveTurnControl`, `ActiveTurnReaper`,
   `GenerationServerRequestWriter`, the server generation loop, raw generation
   admission, and `ServerThreadRuntime::take_thread` / `put_thread` are absent.
9. Existing server contracts and real DeepSeek thread memory, active-turn
   resume, turn control, metadata, list/search, and pagination checks pass.

### P0.3d Deletion Gate

The slice is incomplete if server production code still owns any active-turn
cancel token, generation counter, join handle, resume channel, steer handle, or
thread reclamation loop; if terminal suppression is decided outside the actor;
if interaction replies compare raw generation numbers; or if a live
`RuntimeThread` leaves its actor during normal server operation.

### P0.3d Verification

- `cargo check -p orca-runtime --all-targets` passes, and all 18 runtime-host
  behavior tests pass. They cover per-turn config and task identity, actor-owned
  event sequence and steer admission, generation replacement, terminal commit,
  cancellation, panic recovery, and joined thread/host shutdown.
- All 21 server-runtime contracts and 132 session-server contracts pass. The
  latter include duplicate interrupt/resume, active-turn resume and steer,
  pending interaction cancellation, server EOF cleanup, thread projections,
  metadata, list/search, and paginated turn/item reads.
- `cargo test --workspace --all-targets -- --test-threads=1` passes, including
  767 runtime unit tests, 18 runtime-host tests, 12 task-output tests, 132
  session-server contracts, and 495 TUI tests.
- `cargo clippy --workspace --all-targets` passes with the repository's
  existing warnings and no new warning from the P0.3d implementation or tests.
- The release harness contract test passes, followed by the complete real
  DeepSeek harness: provider summary, headless CLI, malformed-history resume
  and non-reexecution repair, server submit, thread memory, active-turn resume,
  thread read, metadata update, interrupt/resume/steer controls, list filters,
  search, and paginated turn/item reads all pass.
- Deleted-symbol and ownership audits confirm that `ActiveTurnManager`,
  `ActiveTurnControl`, `ActiveTurnReaper`, `GenerationServerRequestWriter`, the
  server generation loop, resettable cancellation, resume mailbox, and
  `ServerThreadRuntime::take_thread` / `put_thread` are absent from production
  code. No release is made at this checkpoint; the next surface migration is
  the TUI onto the proven runtime-host control plane.

## P0.3e1: Joined TUI Worker Supervision

### Structural Problem And Boundary

The server migration made the runtime host the only server turn owner, but the
TUI could not move directly onto that path without first fixing its worker
lifetime. The fullscreen entrypoint discarded its agent `JoinHandle`; each
provider request returned only a receiver while dropping join ownership; a
background-current-turn handoff spawned a second detached completion worker;
and auto-memory used a non-cancellable detached provider call. Exiting the TUI
therefore restored the terminal without proving that any of those workers had
stopped.

P0.3e1 establishes a temporary, explicit ownership boundary:

- `TuiAgentRuntime` owns the existing agent thread, its operation-cancellation
  controller, the shutdown command sender, and a bounded `TuiTaskSupervisor`;
- agent shutdown closes operation admission before cancellation, uses a
  non-blocking wake command, and makes every later operation scope born
  cancelled. A full bounded action mailbox therefore cannot deadlock shutdown
  or admit another turn behind the shutdown boundary;
- terminal teardown disconnects the runtime event receiver and restores the
  user's terminal before cancelling and joining the agent runtime;
- `ProviderStreamTask` owns the stream receiver, cancel token, and join handle.
  Foreground turns join it after `Done`; backgrounding transfers the complete
  task into the supervisor;
- background completion and auto-memory are named supervised tasks. Shutdown
  closes admission, cancels all admitted work, and joins it; completed tasks
  are reaped before they consume capacity;
- auto-memory uses a cancellable streaming provider call and cannot persist a
  note after cancellation;
- background `task_stop` and provider completion settle under one atomic
  `TaskRegistry` lock. A stop request wins over a concurrent success terminal,
  while usage already incurred by the provider remains recorded. Cancellation
  drains an already queued provider terminal before join, so stop cannot discard
  usage merely because the completion consumer had not read `Done` yet.

This is not the final TUI control plane. The existing TUI agent loop still owns
`TuiConversationSession`, borrows the action receiver for interactions, and
uses `OperationCancellation`. The bounded supervisor exists only to make every
provider/completion/auto-memory worker changed in this checkpoint traceable and
to provide the transfer boundary needed by the next actor migration; it must
not become a second permanent runtime host. The pre-existing detached workflow
notification watchers still own `WorkflowBackgroundLaunch` outside this
supervisor and remain an explicit deletion target for the host migration.

### TUI User Value And Compatibility

Closing the TUI no longer leaves its agent, provider, background-current-turn,
or auto-memory work detached. Stopping a background provider task cancels the
network request, waits for the worker, and publishes one stopped terminal even
when stop races with provider completion. Backgrounding still releases the
conversation for the next submit, and foregrounding, approvals, goal usage,
history writes, budget checks, and streamed deltas retain their existing
behavior.

P0.3e1 preserves CLI arguments, TUI keys and flows, server/JSONL contracts,
persistence, and DeepSeek request/model behavior. It is an unreleased
reliability checkpoint, not a feature release.

### P0.3e1 Acceptance And Deletion Gate

1. Explicit shutdown and `Drop` cancel and join the TUI agent thread.
2. Supervised task admission is bounded; shutdown rejects new work and cancels
   and joins every admitted task.
3. Foreground and background provider tasks retain cancel and join ownership;
   disconnect, cancellation, panic unwind, and shutdown cannot detach the
   provider worker.
4. Background stop, completion, usage, history, and callback paths settle once;
   stop wins the terminal race atomically.
5. Auto-memory is cancellable and supervised.
6. Background-current-turn, next submit, foreground, approval, goal, usage,
   and budget behavior remain compatible.

Checkpoint verification passed with `orca-core` 143/143, `orca-runtime`
769/769, and `orca-tui` 506/506 tests; the serial workspace all-targets gate;
workspace Clippy with only pre-existing warnings; the release real-API smoke;
and the complete DeepSeek harness. The real harness covered provider summary,
CLI and history replay/repair, server submit and thread memory, active-turn
resume/control, thread read/metadata, list filters/search, and turn/item
pagination.

At this P0.3e1 checkpoint, the next slice had to replace borrowed interaction
waits with a typed, generation-fenced broker. Those deletion conditions were
closed by P0.3e2 through P0.3e4c: production TUI code no longer owns
`OperationCancellation`, a mutable `RuntimeThread`, the TUI-only agent loop,
borrowed interaction handlers, or unjoined provider/workflow work.

### P0.3b Acceptance Criteria

1. Events emitted by consecutive operations on one actor have one run id and a
   strictly contiguous sequence.
2. A headless session emits one `session.started` before turn events and one
   `session.completed` after `SessionStart` and `SessionEnd` each execute once
   in order.
3. Existing headless JSONL and text contracts remain compatible.
4. A downstream writer or event subscriber failure completes as
   `OperationOutcome::ExecutionFailed`, never `Cancelled`.
5. Host or thread shutdown cancels, joins, and publishes one terminal while
   preserving ownership of the event/thread state until task exit.
6. The headless controller no longer constructs or mutates `RuntimeThread`,
   `InteractiveSession`, or `EventFactory`, and no longer emits session
   lifecycle events or runs session hooks.
7. Focused runtime-host, controller, lifecycle, and exec JSONL tests pass;
   shared-runtime and full workspace serial gates pass.
8. Workspace Clippy passes without new warnings.
9. A real DeepSeek headless JSONL smoke passes through `RuntimeHost` and shows
   one contiguous session sequence and one successful terminal.

### P0.3b Deletion Gate

This slice is incomplete until the old headless session envelope is deleted
from `controller::run_inner`. A helper that keeps controller-owned hooks or
events beside the host is not an acceptable final state. The legacy
turn-without-host APIs remain only for server/TUI and focused internal callers;
their deletion gates remain the later surface migrations listed above.

### P0.3b Verification

- Thirteen runtime-host behavior tests cover actor-owned event continuity,
  headless session and hook order, direct and relayed writer failure,
  headless shutdown ordering, and all P0.3a cancellation/join terminals.
- Controller behavior tests execute the migrated headless path and verify one
  contiguous session lifecycle plus borrowed-writer failure propagation. The
  obsolete source-shape test requiring `RuntimeThread::start` inside
  `run_inner` was deleted.
- The 14 exec JSONL contracts and the focused runtime-lifecycle controller
  contract pass with the existing wire shapes, ordering, exit codes, and
  contiguous sequence assertions.
- `cargo test -p orca-runtime --all-targets -- --test-threads=1` passes with
  779 runtime unit tests, 13 runtime-host tests, and 12 task-output tests.
- `cargo test --workspace --all-targets -- --test-threads=1` passes, including
  130 server contracts and 467 TUI tests.
- `cargo clippy --workspace --all-targets` passes with the repository's
  existing warnings and no warning in the P0.3b implementation or tests.
- The real DeepSeek release harness passes both the headless CLI sentinel and
  malformed-history resume sentinel through `RuntimeHost`; the repaired legacy
  tool call remains non-reexecuted.

## P0.3a Acceptance Criteria

1. Host and actor command channels have explicit finite capacities.
2. Starting a turn returns a stable operation id and an independent completion
   handle.
3. A concurrent `StartTurn` is rejected without replacing or cancelling the
   active operation.
4. Interrupting operation A cannot cancel operation B after B starts.
5. Dropping operation/thread handles does not cancel work; event-consumer loss
   is a typed execution failure, not cancellation.
6. An interrupt terminal is published only after the operation task exits and
   its thread state is reclaimed.
7. `ShutdownThread` cancels and joins active work before actor termination.
8. `RuntimeHost::shutdown` waits for every thread actor; `Drop` cannot detach
   the supervisor.
9. Operation completion is written exactly once, including execution error and
   task panic paths.
10. Tests inspect behavior and public state, not source text or symbol names.
11. Existing `RuntimeThread`, controller, server, and TUI focused tests pass.
12. The shared-runtime full gate passes before the slice is committed.

## Final Deletion Gates

The P0.3e4c deletion gate is complete. The production TUI has no
`OperationCancellation`, mutable `RuntimeThread`, TUI-specific provider/tool
loop, task supervisor, provider facade, detached workflow watcher, or source
shape test protecting those owners. `RuntimeHost` is the only owner of active
and background operation joins, and provider suspension plus non-waiting
workflow execution use typed host-owned tasks.

Broader P0 cleanup remains separately scoped: public borrowed runtime mutation
APIs and unrelated historical source-shape tests can be removed only after
all remaining internal callers migrate to actor commands. They are not
alternate lifecycle authorities for the shipped TUI/server/headless paths.

## v0.3.3 Audit Remediation Boundary

The v0.3.3 audit closes the remaining ownership and responsiveness gaps around
the control plane without changing public wire or persistence formats.

- `ThreadActor` orchestrates four focused state owners:
  `GoalOperationController`, `BackgroundOperationController`,
  `RuntimeCapabilityController`, and `SurfaceCommitController`. Each controller
  owns its pending operations, retries, and terminal bookkeeping; the actor no
  longer edits those maps as shared generic state.
- Goal Store and host session-store calls execute in blocking workers and
  return through bounded typed completion channels. Actor command and
  completion branches never wait synchronously for SQLite or filesystem I/O,
  and the process retains one serialized Goal runtime handle.
- Every foreground operation owns a persisted task-tree root. Interrupt,
  shutdown, and terminal paths request stop for that exact tree, including
  descendants created by attached workers, without cancelling unrelated work.
- The runtime surface exposes an explicit, manifest-bound facade. Provider
  transport consumes provider-neutral tool definitions and policies rather
  than importing concrete tools or branching on tool names.
- `RuntimeHost` constructs and replaces the root MCP registry for each hosted
  session. TUI mention search uses typed runtime access and cannot become a
  second connection owner.
- TUI events carry an immutable session attachment identity. Session switches
  install and project the replacement before retiring the source; stale queued
  events are rejected. Rename commits an exact-revision runtime metadata patch
  and durable title as one operation, restoring the runtime projection if the
  disk write fails.

These boundaries are enforced by behavior, dependency, closed-inventory, and
portable contract tests. Private source layout is not a release contract.

## v0.4.0 ThreadActor Boundary Completion

The remaining actor implementation is split into compile-time modules for
Goal, generation/provider, interaction, operation recovery, and task/workflow
orchestration. The main `ThreadActor` implementation retains mailbox dispatch,
controller lifecycle, and cross-controller transactions.

- `GenerationContextController` is the only constructor for provider response,
  usage, stream-failure, and manual-compaction context events.
- `InteractionController` owns resident interactions, cold-recovery owners,
  origin attachment routing, detach retries, and capability-loss retries.
- `OperationRecoveryController` owns manual-compaction completion, provider
  transfer, live input capsules, reservation expiry, and terminal blocking.
- `TaskWorkflowController` owns background tasks, workflow/provider completion
  retries, approval/control retries, and task ownership transitions.
- `ContextWindowId` is stable for a model-visible epoch, advances only after a
  successful compaction, survives resume, and is isolated by fork identity.

The five actor implementation modules can call only the parent actor's
crate-private orchestration surface. They do not create another mutable runtime
owner or change the TUI, ACP, JSONL, or Headless lifecycle contracts.

## Rejected Alternatives

### Add Another Surface Wrapper

Wrapping the current TUI and server workers in a host while leaving generation,
cancel, and join ownership in each surface would create another compatibility
layer and two facts for terminal state. Rejected.

### Use Unbounded Channels

Operation control traffic is small and must remain bounded under stalled or
misbehaving clients. Unbounded command or event queues would move lifecycle
risk into memory growth. Rejected.

### Cancel On Subscriber Disconnect

Event delivery is observation, not operation ownership. A renderer restart or
client detach must not be interpreted as an explicit user interrupt. Rejected.

### Keep Actor State Borrowed By The Operation

Borrowing `&mut RuntimeThread` across an operation prevents the actor from
owning a joinable `'static` task and obscures shutdown ownership. The thread is
moved into the task and returned on completion instead. Rejected.

## Verification

- Nine focused behavior tests cover concurrent start rejection, stale
  interrupts, independent completion, typed event-subscriber failure, executor
  panic recovery, explicit thread shutdown, host shutdown across actors, host
  `Drop`, and delegation to the existing `ThreadTurnExecutor`.
- `cargo test -p orca-runtime --all-targets -- --test-threads=1` passed with
  778 runtime unit tests, nine host tests, and 12 task-output integration tests.
- `cargo test --workspace --all-targets -- --test-threads=1` passed, including
  130 server contracts and 467 TUI tests.
- `cargo clippy --workspace --all-targets` passed with the repository's existing
  warnings. The new runtime-host implementation and tests introduce no Clippy
  warning.
- A real DeepSeek smoke was intentionally not run for P0.3a because no
  production CLI, server, or TUI path executes through the host yet. It becomes
  mandatory when the first production surface migrates.
