# Orca Production Roadmap

> Goal: evolve Orca into a production-grade DeepSeek-native agent runtime.
> Reference implementations: Codex CLI, Claude Code, and the current Orca codebase.

Last updated: 2026-08-29

The v0.4.0 release adds complete image input across every model selection: TUI
clipboard images, dragged or pasted image paths and `file://` URLs, `@file`
image mentions, and ACP image blocks all enter the same typed runtime surface.
The vision model consumes images directly; `auto`, Pro, and Flash persist a
task-aware vision analysis before the selected coding model continues.
Clipboard reads run off the renderer thread, `[Image #N]` attachments are
atomic, paste-before-send ordering is fenced, and queue editing or rejected
submissions restore the image bytes. Composer previews and message-area
thumbnails open into a zoomable, pannable viewer using terminal-compatible
true-color cells. Native macOS, Linux, Windows, and WSL paths are covered; SSH
without a graphical clipboard uses a remote image path. The same release
completes the `ThreadActor` controller split and gives each compacted context
epoch a stable `ContextWindowId`.

The v0.3.25 release publishes the checkpointable child-agent continuation
lineage together with the runtime-owned durable prompt queue and Codex-style
Goal paste materialization. Queue admission, pause/start control, dispatch,
recovery, rejection, and terminal projection now share one durable revisioned
model across TUI, ACP, JSONL, and Headless. Queue editing uses a
revision-checked runtime delete, failed admission restores the complete prompt,
and previews stream into a bounded 256-character prefix instead of copying a
queued 1 MiB body on every frame. The TUI preserves large ordinary chat paste
bodies behind compact chips and enforces the 1 MiB expanded-input limit without
writing files. `/goal` set and edit actions retain draft paste
identity through the asynchronous action boundary, write only active pastes to
`ORCA_HOME/attachments/<uuid>/pasted-text-N.txt`, and move objectives above
4000 characters into `goal-objective.md`. Path checks confine managed files to
the current attachment directory; uncommitted failures clean the attempt,
while a committed Goal mutation retains the files it references.

The 2026-08-20 checkpointable child-agent continuation slice gives synchronous
subagents, async worker-owned subagents, and Workflow child agents one
runtime-owned lineage model. UUIDv7 continuation, attempt, prompt, and
checkpoint identities are persisted with revision CAS and lease-epoch fencing.
Each safe checkpoint contains the normalized non-system conversation, reasoning
and tool terminal facts, summary state, bounded internal context, budget usage,
turn cursor, compatibility digest, and a canonical payload digest. Resume
rebuilds the current system prompt, restores those durable facts, and appends a
new user prompt; it never restores a Rust future, thread stack, or process.

Tool dispatch persists its replay boundary before execution. `SafeToRetry` and
keyed-idempotent calls can recover from the preceding checkpoint, while an
`IndeterminateAfterStart` call blocks automatic replay until a later safe
checkpoint covers its observed terminal result. Expired owners are reconciled
before legacy active-task recovery: a safe checkpoint becomes `Suspended`, and
an owner without a safe replay boundary becomes `Indeterminate`. Async task and
continuation leases renew under the same owner fence, stale workers cannot
commit checkpoints or terminals, and worker-spawn failures settle prepared
attempts without consuming a resume opportunity.

Workflow completed-result caching remains the first resume fast path. A failed
or interrupted Workflow child with a compatible safe checkpoint resumes the
same child conversation; incompatible context and indeterminate side effects
fail closed instead of entering transient retry. Task summaries and
`subagent_status` publish the same continuation id, attempt id, checkpoint id,
resumable, and indeterminate fields to TUI, ACP, JSONL, and headless consumers.
Worktree continuation inherits the recorded path only if it still exists and
never silently creates a replacement. Retryable resumable Workflow attempts
retain that path until the continuation settles, so a clean worktree is not
removed before the next attempt can resume it.

The 2026-08-18 Durable Interaction Broker completion closes the pending-store
deletion gate. Tool Approval, Permission Request, User Input, and MCP
Elicitation now pass the complete TUI, ACP, JSONL, and Headless 4×4 matrix.
Stable route epoch, response token, grant, and operation fence selectors are
exact and fail closed. Tool Approval records an `InvocationStarted` receipt;
Permission retries only before the protected side effect; and answered User
Input and MCP interactions resume through stable durable continuation
operations. Missing, executing, unsafe, unsupported, and stale-context cold
recovery durably fail closed. The `RuntimePendingInteractionStore` source file,
its crate export, and `with_pending_interactions` compatibility builder are
deleted, with zero old symbols in production or test source. The validator,
runtime all-targets checks, and TUI checks pass. This is a breaking Rust source
API removal for callers of the former compatibility module or builder.

The 2026-08-10 headless resume slice makes "restore a headless execution" a
first-class CLI capability, following Codex's `exec resume` design while keeping
Orca's typed termination and durable session identity. `orca exec` now accepts
a `resume` subcommand: `orca exec resume <SESSION_ID> [PROMPT]...` continues
the saved conversation by id, prefix, or `latest`, and
`orca exec resume --last [PROMPT]...` picks the most recent recorded session.
The shared run options (`--provider`, `--output-format`, `--cwd`, `--mode`,
`--model`, `--api-key`, `--base-url`, `--verifier`, `--max-budget`) are global
flags that apply in either position, and combining the subcommand with
`--resume`/`--fork`/`--continue` is rejected. The original session record is
immutable: resume appends to the same transcript with a fresh budget scope, so
the previous run's consumption records stay durable while the new invocation
owns its own `max_budget` accounting. On headless exit, `session.completed`
now carries the durable `session_id` when history was recorded (JSONL mode),
and text mode prints `To continue this session, run: orca exec resume
<session-id>` for every non-success terminal — so a budget-exhausted run (exit
code 4, `status=budget_exhausted`) is visibly distinct from a failure and tells
the caller exactly how to continue. This changes no persisted transcript
schema and no server protocol.

The same line adds Claude Code's message-boundary restore: `--resume-at
<MESSAGE_ID>` (on both the `resume` subcommand and `--resume`/`--continue`)
restores the conversation only up to a persisted conversation item id — the
durable boundary — so a later prompt cannot replay uncommitted work past a
chosen point; unknown boundaries fail closed. It also adds Grok-style budget
scope separation at the checkpoint level: when a headless session stops at
`budget_exhausted`, the runtime persists a typed `session.checkpoint` record
(status, reason like `max_inner_turns`/`cost_budget_exhausted`, aggregate
budget consumed, the last committed message id, the current task plan, and
`resumable: true`) before the terminal projection, so the resume command can
start from the last committed boundary with a new budget. Uncommitted tool
calls are still distinguished on restore via the existing indeterminate
compatibility repair — Orca promises resumable, not exactly-once, execution.
Conversation rewind is supported through `--resume-at`; file rewind is
explicitly not promised because Orca does not snapshot external workspace
state (Grok's principle: only restore durable facts, never pretend external
side effects are rewindable).

The 2026-08-10 P2.4 prompt-cache identity slice now canonicalizes DeepSeek
plain and beta-strict tool payloads by name before the 128-tool cap, and derives
a versioned, domain-separated SHA-256 checkpoint from the actual lowered
messages, primary tool payload, endpoint, model, and reasoning mode. Runtime
history appends the content-free `provider.prompt_cache_checkpoint` record only
for real DeepSeek turns immediately before dispatch; transcript restoration and
fork initialization ignore it. Focused provider/runtime tests cover permutation,
prefix extension, changed system/tools, serialization privacy, round-trip
recovery, and fork isolation. The latest credential-gated verifier completed
two real requests with `second cache_tokens=1024` (the first also reported
`cache_tokens=1024` because the remote prefix was already warm), confirming a
non-zero DeepSeek cache hit without exposing credentials.

Current baseline: v0.4.3 shows the running queue task in the TUI queue
preview and auto-queues submissions made while the session is busy,
routing them through the durable prompt queue instead of failing the
handoff, with queue interactions wired into the hosted controller.
v0.4.2 indexes surface commit batches by id so
recovery of long recorded surface logs scales linearly instead of
quadratically, with a bounded-time regression gate. v0.4.1 batches
provider step commits with deferred
surface refresh, caches token counts, shards the reducer state, and
coalesces consecutive delta events before renderer dispatch; it also
hardens sandbox metadata handling so symlinked or forged metadata roots
cannot widen grants, and classifies workflow active state by exclusion.
v0.4.0 adds DeepSeek vision and the complete TUI/ACP image
input path, completes the focused `ThreadActor` controller split, and adds
stable context-window epochs. v0.3.26 makes the unsandboxed shell permission an
explicit, reusable, session-scoped capability: the turn permission overlay
carries it with merge, delta, and apply semantics, bash consumes it before
prompting, grants are recorded only on allow, and session-scope allow
responses persist it into thread settings and JSONL metadata so recorded
sessions restore it into every new turn without re-asking. It builds on
v0.3.25's checkpointable child-agent continuation,
runtime-owned durable prompt queues, and Codex-style Goal paste
materialization. v0.3.25 builds on v0.3.24's durable four-interaction broker and
v0.3.23's thread-owned interactive `exec_command` and
`write_stdin` sessions with optional PTY, raw control input, bounded output,
task-based process-tree control, and a single-owner background supervisor that
settles natural exits and external stop requests without another poll. It also
injects exactly-once bounded completion notices before the next model turn
unless the terminal result was already observed. This builds on v0.3.22's
interactive configuration, structured question dialog, and terminal-native
composer editing; v0.3.21's explicit TUI state ownership and legacy task
recovery; and v0.3.20's bounded,
project-scoped automatic memory, v0.3.19's bounded release gate, v0.3.13's
headless resume, and v0.3.12's runtime-owned Side Conversations. Prior releases
added v0.3.8's remaining-context
visibility, one-to-one
compaction replay, pending-steer recovery, durable paged subagent results,
proactive background-task completion notices, stdio MCP reconnect, stale
JSONL event-sequence writer rejection, and run-level workflow token budgets.
It also adds the searchable `/skills` picker and makes slash settings read the
committed runtime state. These changes build on v0.3.7's explicit
delegation-policy inheritance, cross-process transcript write serialization,
durable retry and truncation diagnostics, revision-safe TUI projection,
ordered assistant stream replay, and full task-registry publication before
session completion on headless and interactive surfaces, plus native Windows x64 and ARM64 support across the
CLI, TUI, shell sessions, sandboxing, update flow, persistence, npm packages,
release archives, and CI. Shell resolution preserves PowerShell 7, Windows
PowerShell, cmd.exe, and explicit Git Bash dialects. PowerShell 7 is discovered
from both `PATH` and its standard installation directory; restricted sessions
fall back to cmd.exe rather than running Windows PowerShell 5.1 in AppContainer
ConstrainedLanguage. ConPTY owns interactive terminal I/O and resize; Job
Objects own process-tree cleanup; AltGr and clipboard input follow Windows
behavior.

The current unreleased session-picker line uses a paginated, searchable SQLite
summary index. Its filesystem boundary admits only regular `.jsonl` and
`.jsonl.zst` transcripts, never follows symlinks, and evicts indexed paths that
become non-regular. History readers also open no-follow/nonblocking handles and
verify the resulting handle is regular, so a concurrent FIFO replacement cannot
stall a picker or transcript read. Stateless server checks distinguish recorded
catalog/transcript state from the empty index infrastructure created by listing.
This changes no public protocol, SQLite schema, or persisted transcript format.

The v0.4.4 TUI state-owner convergence slice completes the last large local
state boundary left after the renderer, input, interaction, queue, projection,
and runtime-controller work. `protocol.rs` now owns `TuiEvent`, `UserAction`,
interaction keys/responses, lifecycle values, and attachment values;
`state_reducer.rs` owns `AppState::update` and event-dispatch helpers;
`TranscriptState`, `InteractionState`, and `ViewportState` own only fields with
independent invariants. `AppState` remains the composition root for cross-owner
transitions, while `types.rs` is reduced to aggregate construction, shared
operations, and precise compatibility re-exports. Owner-specific tests move
with those modules, leaving the state integration suite for lifecycle and
projection behavior that genuinely crosses owners. New contributors should
start from the owner module named by the invariant they are changing, not from
the aggregate.

The current TUI convergence slice gives input-history persistence and
draft-restoring recall one module owner. `AppState` remains the single aggregate
fact source for the history vector, navigation cursor, and saved draft; only the
local I/O and state-transition policy move out of `types.rs`. Key routing,
history JSONL, CLI, server/JSONL, and session persistence behavior remain
unchanged.

The next TUI convergence slice gives queued follow-up facts one private
`QueuedSubmissionState` owner in `queued_input.rs`. The pending FIFO, in-flight
admission fence, autosend flag, last error, and next id now change only through
owner transitions; channel dispatch remains in `queued_input_actions.rs`, while
`AppState` still coordinates global status, transcript projection, input-history
recording, and typed `UserAction` construction. The UI reads a bounded owned
preview rather than the queue. Queue state remains intentionally process-local,
so restart begins empty and makes no durability or exactly-once claim. TUI key
flows, runtime events, CLI, server/JSONL, and persisted formats are unchanged.

The edit-highlight convergence slice gives workspace/theme configuration,
worker runtime, pending-job coordination, applied revision styles, and test
hooks one private `EditHighlightState` owner in `edit_highlight.rs`. AppState
still owns transcript messages, revision advancement, and render-cache
invalidation; the renderer receives only an immutable applied-style projection.
Runtime replacement raises a shutdown fence, closes the job sender and result
receiver, then joins its bounded worker before returning. The fence is checked
before and during queue coalescing, so reconfiguration cannot leave an unowned
old worker, drain a queued backlog, or publish its derived styles later.
Highlight state remains process-local and rebuildable;
ordinary diff rendering is the silent fallback after malformed input, stale
identity, spawn/channel failure, or restart. TUI output, runtime events, CLI,
server/JSONL, and persisted formats are unchanged.

The surface-metrics convergence slice makes the authoritative reducer snapshot
the only production TUI update for usage and context facts. One private
`SurfaceMetricsState` now owns usage, usage revision, context revision, used
tokens, and limit tokens; the duplicate `TuiEvent::UsageUpdated` and
`TuiEvent::ContextUpdated` paths and their `context_observed` arbitration flag
are deleted. Context compaction still constructs its dedicated lifecycle event
before the batch snapshot. The existing manual-compaction delivery boundary
then commits that snapshot before publishing `Compacted` and the terminal event,
so the notice observes the committed metrics. CLI,
server/JSONL, ACP, runtime surface events, and persisted formats are unchanged.

The Goal projection convergence slice makes the authoritative reducer snapshot
the only production TUI update for the committed Goal fact. One private
`SurfaceGoalProjectionState` owns the displayed Goal, its accepted surface
cursor, and presentation deduplication; lower cursors, contradictory equal
cursors, and different incarnations before reset cannot replace it.
`GoalStatus` is presentation-only, while edit, clear, and pause re-read a fresh
post-commit snapshot and prove that it covers the mutation cursor. The granular
`GoalUpdated`/`GoalCleared` events and public mutable `AppState.current_goal`
field are deleted; renderers use the immutable `current_goal()` query. Session
identity, workflow, and operation projection remain the next convergence
boundaries. CLI, server/JSONL, ACP, runtime surface events, Goal persistence,
and persisted transcript formats are unchanged.

The session-identity projection convergence slice makes the authoritative
runtime surface snapshot the only production TUI source for the attached
thread's title and optional recorded session id. One private
`SurfaceSessionProjectionState` owns those facts, the accepted thread cursor,
and once-per-cursor rename/fork presentation. Ephemeral threads keep their
runtime title without pretending to be resumable; lower, contradictory equal,
and cross-thread ordinary projections are rejected before any other snapshot
owner changes. New, resume, fork, and Side transitions publish complete reset
snapshots, while newly started durable threads must pass a snapshot/id preflight
before installation. The granular identity/rename/fork events, public mutable
AppState fields, caller-authored reset titles, and app-loop identity shadow are
deleted; renderers, commands, picker actions, and exit policy use immutable
queries. Operation and live workflow-task projection are completed by the
following slices. CLI, server/JSONL, ACP, runtime surface events, session
persistence, and transcript formats are unchanged.

The foreground and recoverable operation projection convergence slice makes the
authoritative runtime surface snapshot the only production TUI source for the
active and recoverable operation ids. `SurfaceProjectionState` derives
recovery eligibility from `SurfaceSnapshot::recoverable_user_operation()`, and
one private `SurfaceOperationProjectionState` owns both ids with its accepted
cursor. It rejects stale, cross-identity, and contradictory equal-cursor
observations before any projection owner changes; reset validation prevents an
invalid recovery pair from clearing a live session. Recovery prompt visibility
and its notice are presentation effects of an accepted snapshot, so the
granular `RecoveryAvailable` event and mutable AppState operation fields are
deleted. Commands use immutable queries. The following terminal-history slice
admits a strict subset of pre-surface registry rows for recorded threads;
non-recorded, active, approval, failed/retryable, and rich rows remain hidden
and non-actionable instead of adding a TUI cache. CLI, server/JSONL, ACP,
runtime surface events, operation persistence, and transcript formats are
unchanged.

The 2026-08-16 workflow-task projection slice makes the accepted runtime surface
snapshot the only production TUI source for task, workflow, and subagent rows.
`SurfaceProjectionSynced` now carries the one post-commit task replacement;
`WorkflowTasksUpdated`, `WorkflowTaskUpdated`, startup/monitor list duplication,
and registry-derived TUI fallbacks are deleted. Stop, foreground, background
approval, and approval-resolution actions return or publish post-commit snapshots,
and ephemeral Side foregrounding uses the same typed task fence, hydration, and
presentation ownership as recorded sessions. The task panel remains process-local
for sorting, selection, reveal, and foreground return. Active, approval,
failed/retryable, workflow, subagent, shell, and monitor registry-only recovery
remains open and fail-closed. CLI, server/JSONL, ACP, runtime surface events,
task persistence, and transcript formats are unchanged.

The 2026-08-17 legacy terminal-task reconciliation slice adds the first
receipt-backed cold import without reintroducing a TUI registry read. During
recorded-thread startup, `TaskRegistry` holds the session lock while issuing an
opaque digest over sorted eligible rows; the runtime coordinator admits exactly
missing revision-one MainSession rows in Completed, Stopped, or Cancelled state
through one dedicated `TaskPatch::Reconciled` authority. Ordinary actor commits
cannot use that patch, prepared recovery accepts only the same non-actionable
append-only shape, and the reducer cannot omit or change an existing surface
task. Empty and repeated imports consume no reconciliation commit; unreadable
receipts and exhausted bounded ledger retries fail startup without changing the
registry. The TUI receives the result only through `SurfaceProjectionSynced`.
Public task, CLI, server/JSONL, ACP, and persisted schemas are unchanged.

The 2026-08-17 legacy active-task adoption slice closes the safe registry-only
Running MainSession case without fabricating continuation. A locked receipt
authorizes exactly one five-event operation/generation/task/transfer group per
missing row through a dedicated commit authority, but only when the recovered
surface has no existing operation lineage; a legacy row has no operation id and
cannot safely be distinguished from an unmaterialized typed-operation mirror.
The adopted generation is
non-replayable with an unavailable capsule, so existing cold recovery records
`AbortedByRuntimeRestart`; terminal task reconciliation then stops the surface
task before the compatibility mirror changes. Prepared recovery accepts only
the same canonical shape, append failure is bounded and non-mutating, and
repeated restart is idempotent. The TUI still consumes tasks only through
`SurfaceProjectionSynced`. Queued, paused, stopping, approval, failed/retryable,
workflow, subagent, shell, and monitor reconstruction remains the next cold
migration boundary. Public task, CLI, server/JSONL, ACP, surface-event, and
persisted schemas are unchanged.

The 2026-08-16 hosted Goal orchestration ownership slice moves stateless
turn-request construction, typed ordinary-turn dispatch, operation-error
shaping, and Goal lookup/run adapters into `hosted_runtime.rs` and
`hosted_goal.rs`. The controller still owns the action loop, Side/session
attachments, and lifecycle decisions. Runtime-thread installation, shutdown,
preloaded-session clearing, and config updates remain one transaction in the
hosted session lifecycle owner. This changes no runtime surface, persistence,
CLI, server/JSONL, ACP, or user-visible Goal behavior.

The 2026-08-16 hosted session projection ownership slice moves stateless typed
snapshot conversion, attached reset/history publication, runtime-ready
publication, and saved-history eligibility/fallback into `hosted_session.rs`.
The controller still owns thread installation, replacement, shutdown/reaping,
and attachments. This changes no runtime surface, persistence, CLI,
server/JSONL, ACP, or user-visible session behavior.

The 2026-08-16 hosted session lifecycle ownership slice moves hosted thread
startup, replacement preflight, installation, asynchronous reaping, new/fork/
saved-session switching, and saved-session list refresh into
`hosted_session_lifecycle.rs`. The controller still owns event routing,
attachments, the controller loop, and final shutdown. This changes no runtime
surface, persistence, CLI, server/JSONL, ACP, or user-visible session behavior.

The 2026-08-16 hosted settings ownership slice moves settings-intent patch
translation and the attached/unattached settings application transaction into
`hosted_settings.rs`. Live-thread settings still commit through the typed
runtime surface before the effective result is mirrored into TUI config;
pre-thread settings still update only the startup config. The controller keeps
action selection and plan-implementation sequencing. This changes no runtime
surface, persistence, CLI, server/JSONL, ACP, or user-visible settings behavior.

The 2026-08-16 hosted submission ownership slice moves the submitted-turn
transaction into `hosted_submission.rs`: missing-thread startup, readiness
publication, bound-mention expansion, queued rejection identity, ordinary or
Goal-aware typed dispatch, and the existing desktop completion notification.
The controller keeps action selection, Side/config selection, queued-input
scheduling, plan gating, and final shutdown. This changes no runtime surface,
persistence, CLI, server/JSONL, ACP, or user-visible submission behavior.

The 2026-08-16 hosted latest-active Goal recovery ownership slice moves the
candidate-session recovery transaction into `hosted_session_lifecycle.rs`.
Latest active Goal discovery, transcript loading, candidate runtime startup,
typed Goal validation, old-thread retirement, config/preloaded mutation,
recovered-approval publication, and continuation launch remain one ordered
transaction. The controller still chooses when Goal resume runs and owns
attachments, action routing, and final shutdown. This changes no runtime
surface, persistence, CLI, server/JSONL, ACP, or user-visible Goal behavior.

The 2026-08-16 hosted Goal action ownership slice moves the six show/set/edit/
clear/pause/resume transactions into one `hosted_goal.rs` entry point. The
controller now maps existing `UserAction` variants into a crate-private command
without copying Goal state. Thread startup, runtime-ready and notice ordering,
committed projection publication, latest-active recovery delegation, and
Goal-run error shaping remain unchanged. The controller still owns Side
restrictions, action selection, attachments, and final shutdown. This changes
no runtime surface, persistence, CLI, server/JSONL, ACP, or user-visible Goal
behavior.

The 2026-08-16 hosted session action ownership slice moves the eight new/fork/
resume/rename/archive/delete session transactions into one
`hosted_session_lifecycle.rs` entry point. The controller maps the existing
`UserAction` variants into a crate-private command while candidate preflight,
thread replacement, attachment rotation, reset/history projection, picker
refresh, and current-session protection remain one lifecycle-owned sequence.
Side switching and final controller shutdown stay in `app.rs`. This changes no
runtime surface, persistence, CLI, server/JSONL, ACP, or user-visible session
behavior.

The 2026-08-16 hosted Side action ownership slice moves Side start/toggle/close
into one `hosted_side.rs` entry point. Candidate startup and rollback,
ephemeral config, attachment rotation, reset/history projection, deferred
parent event replay, background presentation rebind, and bounded child
shutdown now stay with the existing Side parent owner. The generic attached
sender rotation helper moves to `attachment_routing.rs`, avoiding a new
production dependency cycle with session lifecycle. This changes no runtime
surface, persistence, CLI, server/JSONL, ACP, or user-visible Side behavior.

The 2026-08-16 hosted context action ownership slice moves Remember, Compact,
and Backtrack into one `hosted_context.rs` entry point. Recorded-thread startup,
runtime-ready publication, memory/pin partial-success ordering, manual
compaction error shaping, and restored-prompt translation now stay together
while typed runtime surface actions remain the only mutation authority. This
changes no memory/transcript schema, runtime surface, persistence, CLI,
server/JSONL, ACP, or user-visible context behavior.

The 2026-08-16 hosted workflow action ownership slice moves saved-workflow
launch into one `hosted_workflow.rs` entry point. Hosted thread startup and
title selection, readiness publication, typed launch rejection, immediate
success completion, and desktop-notification policy now stay together while
workflow admission, task state, and terminal notifications remain runtime-owned.
This changes no workflow schema, runtime surface, persistence, CLI,
server/JSONL, ACP, or user-visible workflow behavior.

The 2026-08-16 hosted operation recovery ownership slice moves explicit
ResumeOperation and CancelOperation transaction shaping into one
`hosted_operation.rs` entry point. The controller now maps the existing
operation id into a crate-private command; missing-thread rejection and
immediate typed-action failure prefixes stay unchanged. Recovery admission,
stale fencing, cancellation, terminal settlement, waiters, retries, timeouts,
disconnects, and restart remain runtime-owned. This changes no operation
schema, runtime surface, persistence, CLI, server/JSONL, ACP, or user-visible
recovery behavior.

The 2026-08-16 hosted plan implementation ownership slice moves the ordered
approved-plan transaction into one `hosted_plan.rs` entry point. The existing
approval mode commits through `hosted_settings` before
`PlanImplementationStarted`, and the original prompt then enters
`hosted_submission`; settings rejection releases the dispatcher-prearmed
activation without claiming implementation began. The controller now only maps
the existing user action. This changes no plan prompt, settings or operation
authority, runtime surface, persistence, CLI, server/JSONL, ACP, or user-visible
plan behavior.

The 2026-08-16 hosted task action ownership slice consolidates task stop,
foreground return, and background approval resolution in
`background_tasks.rs`. The controller now only maps the existing user actions;
task/interaction fences, response routing, operation presentation, retries,
timeouts, and terminal settlement remain runtime-surface owned. Denied or
failed background approval paths now release the dispatcher-prearmed surface
activation, preventing an idle interrupt from leaking into the next operation.
This changes no task or interaction schema, persistence, CLI, server/JSONL,
ACP, or public API.

The 2026-08-16 pending interaction input admission slice keeps foreground
user/MCP answers on their prioritized runtime response path while repairing
renderer preflight. Waiting answers now bypass slash-command execution, MCP
Form JSON is validated before optimistic state cleanup, and URL mode retains
its private projection so empty acceptance can submit `{}`. Invalid JSON keeps
the exact pending interaction and composer available for retry. Runtime
interaction fencing, response mutation, retries, disconnect handling, and
terminal settlement remain unchanged; no action/event, persistence, server,
ACP, CLI, or public API changed.

The 2026-08-16 interaction response acknowledgement slice closes the remaining
renderer loss window after local input admission. The prioritized dispatcher
publishes a bounded non-blocking crate-private committed/stale/failed receipt, while the
frame loop retains at most one key-matched pre-expansion composer snapshot.
Runtime/uncommitted failures restore the exact pending key, MCP mode, visible
text, mentions, paste payloads, and atomic skill tokens; committed, stale,
newer-interaction, terminal, and reset paths retire the snapshot. Runtime
interaction mutation/fencing and all action/event, persistence, server, ACP,
CLI, and public API contracts remain unchanged.

The 2026-08-16 renderer runtime-event ownership slice gives attachment
admission, deferred initial-prompt consumption, special renderer event routing,
and mention-search synchronization one `RendererRuntimeEventOwner`. The frame
loop still owns terminal/input scheduling and drawing, while
`runtime_event_actions` remains the admitted-event reducer. Stale attachments
cannot consume the deferred prompt or mirror settings; the first admitted
history event still hydrates before submitting that prompt exactly once.
Mention workers retain their existing generation fences and shutdown path.
This changes no runtime surface, event/action payload, persistence, CLI,
server/JSONL, ACP, public API, or visible TUI behavior.

The 2026-08-16 renderer frame ownership slice gives frame timing,
edit-highlight admission, animation and copy-notice coordination, resume
redraw, bounded input/runtime iteration scheduling, clipboard consumption,
pending terminal presentation output, drawing, and successful-draw
acknowledgement one `RendererFrameOwner`. Input wake and routing,
runtime-event reduction, terminal lifecycle, and hosted runtime shutdown keep
their existing owners. The 16/80 ms cadences, batch limits, best-effort title
and notification output, final copy-notice redraw, and draw-error dirty-state
behavior remain unchanged. No runtime surface, event/action payload,
persistence, CLI, server/JSONL, ACP, public API, or visible TUI behavior
changed.

The 2026-08-16 terminal session startup ownership slice gives terminal/input
assembly and its pending-to-activated transition one `PendingTerminalSession`
owner. Input probing and terminal lease acquisition still precede hosted-agent
startup; ratatui construction and the startup clear still happen only after the
agent is ready. Agent-startup failure still finishes pending input with the
same error precedence. Initial title/draw and exit cleanup remain in
`presentation.rs`, while qwertty modes, signals, leave, and join remain in
`input_runtime.rs`. No runtime surface, event/action payload, persistence,
CLI, server/JSONL, ACP, public API, terminal escape sequence, or visible TUI
behavior changed.

The 2026-08-16 renderer input-wake ownership slice gives renderer-side input
receiver ownership and the terminal suspend/resume handshake one
`RendererInputWakeOwner`. The existing biased control/focus/ordinary priority,
64-event ordinary cap, pointer-motion filter, suspend acknowledgements,
repeated-suspend handling, resume callback, and exact disconnect errors remain
unchanged. `input_wake.rs` keeps the stateless selection primitive,
`RendererFrameOwner` keeps resume rendering, and `app.rs` keeps semantic
scroll/focus/paste/resize/mouse/key routing. No runtime surface, event/action
payload, persistence, CLI, server/JSONL, ACP, public API, terminal escape
sequence, or visible TUI behavior changed.

The 2026-08-16 renderer input-routing ownership slice gives semantic scroll,
focus, insert-escape, paste, resize, mouse, synthetic-Enter, and real-key
sequencing one `RendererInputRouter`. The existing low-level action modules
still own their policies, while `app.rs` keeps input coalescing, interaction
acknowledgements, mixed input/runtime iteration, frame presentation, terminal
cleanup, and hosted runtime shutdown. Short-circuit order, timestamps, clear
errors, exit codes, collaborators, input priority/capacity, terminal behavior,
and visible shortcuts remain unchanged. No runtime surface, event/action
payload, persistence, CLI, server/JSONL, ACP, or public API changed.

The 2026-08-16 renderer interaction-acknowledgement ownership slice gives the
renderer-side acknowledgement receiver, non-blocking FIFO drain, and
non-empty batch result one `RendererInteractionAckOwner`. The dispatcher keeps
bounded production and overflow policy, `runtime_event_actions.rs` keeps every
single-ack state/composer/Vim/status effect, and `app.rs` still marks the frame
dirty once for each non-empty batch. Capacity, payloads, reducer behavior,
frame scheduling, shutdown, protocol, persistence, and visible TUI behavior
remain unchanged.

The 2026-08-16 renderer runtime-inbox ownership slice gives the bounded
runtime-event receiver, borrowed non-blocking FIFO iterator, and explicit
pre-agent close boundary one `RendererRuntimeInboxOwner`. Frame scheduling
still processes input before runtime events with the same 256-event cap;
`RendererRuntimeEventOwner` still owns attachment admission, event effects,
mention synchronization, and mention-worker shutdown. Terminal cleanup still
precedes mention shutdown, inbox close still releases capacity-blocked
producers before agent shutdown, and all event/action, protocol, persistence,
server, ACP, public API, and visible TUI behavior remain unchanged.

The 2026-08-16 renderer iteration-event routing ownership slice gives the
typed input-vs-runtime branch and its exact result translation one
`RendererIterationEventRouter`. Input events still delegate once to
`RendererInputRouter` with the same timestamp and terminal-clear callback,
including unchanged exit codes and `io::Error` propagation. Runtime events
still delegate once to `RendererRuntimeEventOwner` and continue with
`Ok(None)`. `RendererFrameOwner` keeps input-first ordering, batch limits,
and iteration short-circuiting. At that slice boundary, the outer loop still
kept composer synchronization, exit handling, presentation, and shutdown; the
following renderer-loop slice moves only that foreground sequence. No
event/action, runtime surface,
persistence, server, ACP, public API, or visible TUI behavior changed.

The 2026-08-16 renderer-loop ownership slice gives the complete foreground
iteration cycle one `RendererLoopOwner`. It preserves expired insert-escape
flush, frame preparation, prioritized input/resume, acknowledgement draining,
input-first mixed dispatch with the same 256-event runtime cap, composer
synchronization, exit-before-presentation, clipboard output, pending terminal
output, and drawing in their exact order. `app.rs` retains initial title/draw,
terminal cleanup, mention-worker shutdown, inbox close, and hosted-agent
shutdown. The resume helper is generic only across the existing crate-private
ratatui backend boundary so the owner is exercised with `TestBackend`. No
event/action, runtime surface, persistence, server/JSONL, ACP, public API,
terminal escape, or visible TUI behavior changed.

The 2026-08-15 plan-panel ownership slice moves only process-local TUI
presentation facts behind one private `PlanPanelState`: the live structured
plan and its failed-update marker. Existing `PlanUpdated` and legacy
`HistoryLoaded.plan` payloads remain the inputs because legacy structured-plan
hydration has not moved into the runtime surface. This deliberately changes no
runtime plan persistence, history format, surface projection, protocol, or
renderer behavior.

The 2026-08-15 workflow-panel ownership slice moved task-row presentation and
selection behind one private `WorkflowPanelState`; its earlier wording about
full/single-task event inputs is superseded by the 2026-08-16 snapshot-only task
projection slice above. The panel still owns sorting, selected-id retention,
background/approval reveal, and foreground-return effects, while cold legacy
registry reconciliation remains outside the TUI boundary.

The 2026-08-15 Side background reentry slice keeps attachment rotation strict
while restoring the active Side task panel after a return from main. Parent to
Side activation still retires the previous Side sender and publishes its reset
and history before accepting new events. It then replaces only the presentation
monitor for each live runtime-owned background operation with one bound to the
new attachment. The task remains backgrounded, stale output remains fenced, and
explicit foreground control is unchanged; no runtime ownership, protocol, or
persisted data changes.

The Windows sandbox uses restricted tokens or AppContainer according to the
requested filesystem and network policy. The PowerShell installer verifies the
release checksum, installs the runner and setup helper, and can provision,
repair, or remove a workspace capability receipt. Missing setup and unsupported
domain-restricted network policy fail closed. Atomic replacement and OS locks
cover the runtime's durable stores, while native x64 and ARM64 runners execute
platform contracts and the full workspace test suite.

Historically, the 2026-08-07 TUI single-surface interaction slice removed the
process-local TUI interaction broker, four legacy interaction adapters, and the
legacy hosted turn runner. Production turns and interaction tests now use the
typed runtime surface and its supervised presentation control. The 2026-08-08
runtime pending-store slice removed that process-local map from `RuntimeHost`
ownership.
At that checkpoint, the public store and `with_pending_interactions` builder
remained as documented, source-compatible no-op shims pending legacy caller
migration and durable broker recovery evidence. That compatibility state was
superseded by the completed 2026-08-18 deletion.

The historical 2026-08-10 pending-store gate audit confirmed the first two
ownership and recovery conditions, but did not delete the shim: legacy
`HostedTurnRequest`/Goal continuation workers were still production paths used
by TUI, ACP, and controller code, and `cargo-semver-checks` was unavailable in
that checkpoint's environment. The report at
`docs/reports/2026-08-10-pending-store-deletion-gate.md` now records the
superseding 2026-08-18 completion and breaking API impact.

The 2026-08-08 Goal interaction-settlement slice keeps TUI approval semantics
typed across the runtime-to-Goal boundary. Allow executes the approved tool and
settles the operation and outer turn once. Deny persists `ApprovalRequired`,
performs no tool side effect, leaves no in-flight run, and requires explicit
resume with a fresh run, operation, and interaction fence. Operation and durable
Goal usage now share one rounded micro-dollar conversion, while the surface Goal
accumulates outer-turn deltas to remain consistent with SQLite across resume.
This changes no CLI, TUI, server JSONL, or persistence schema.

The 2026-08-13 auto-memory v2 slice supersedes the earlier synchronous
final-response append. A durably successful verified root turn now enqueues an
idempotent project job; a session-owned cancellable worker leases and retries
jobs, strictly parses typed candidates, and commits an atomic authoritative
JSONL ledger plus repairable Markdown and SQLite FTS5 projections. New turns
retrieve only bounded relevant candidates with provenance as non-transcript
internal context; stale or corrupt indexes rebuild from the ledger and any
index error falls back to lexical recall.
Exact turn evidence survives provider suspension and process restart through
durable `turn_id` records. Manual `/remember` remains a separate fact source,
and `auto_memory = false` disables automatic capture and recall.

The 2026-08-09 TUI terminal-wait slice now carries a one-shot process-local
cancellation signal through the runtime surface. Runtime-owned terminal waiters
are retired as typed `WaitCancelled` results, terminal commits retain precedence,
and the TUI cancels and joins every waiter before background handoff, projection
failure, subscription sealing, or recovery failure returns. Existing convenience
waiters remain uncancelled and source-compatible; no operation state, persistence
record, or external protocol changed.

The 2026-08-09 MCP SSE wire slice adds socket-level coverage for unhandled
elicitation decline, malformed-request `-32602` responses, and cancellation while
the elicitation response POST is held open. Fixtures verify original request ids,
peer closure, and final tool results while preserving the existing bounded
single-response path for initialize, list, and resource methods.

The 2026-08-08 headless trajectory contract now exercises the existing default
`128` inner-turn boundary through the real `orca exec` binary. A repeated-tool
run emits exactly 128 admitted turns and tool terminals, returns exit code `4`
with one `session.completed(status=budget_exhausted)`, and persists the same 128
flattened tool-terminal records without inventing an unadmitted 129th call. The
fixture and contract use the existing controller, JSONL sink, and SessionWriter
owners; no second trajectory source or protocol shape was added.

The follow-up headless projection contract now compares the ordered streamed and
persisted terminal tuples (`id`, `status`, `kind`, `exit_code`) for every one of
those 128 calls. The real-binary boundary test therefore verifies not only
counts and ids but also that the event and resume projections describe the same
terminal outcomes.

The 2026-08-09 Side Conversation slice adds one runtime-owned, process-local
`EphemeralAttached` child created from an atomic parent snapshot. `/side` opens
the separate transcript, `Ctrl+/` switches between child and parent without
stopping the parent's work, and `Ctrl+C` performs a bounded child shutdown and
join before restoring the parent. Side history, goals, memory, settings, and
tool results never merge into or create a durable parent session. Side runs in
Plan/read-only mode, so it cannot race a running parent with workspace or git
mutations; mutation requests must be performed from main. Parent events remain
observable as typed status while Side is visible, navigation is rejected until
the attached child is closed, and controller shutdown always settles the child
before the parent. A PTY contract exercises open, independent response,
toggle, close, continued parent input, and the single durable-session boundary.

The same release fences TUI provider-response projection by logical turn,
response item id, and channel. Reconciliation removes incomplete assistant and
reasoning tails before replay, preventing an older response or abandoned stream
from replacing the active response after hydration.

Earlier v0.2.56 kept the executable as a thin parser and forwarding layer while
`orca-runtime` and `orca-tui` took ownership of configuration, launch, update,
history, trust, workflow, protocol, and worker behavior. Stateless JSONL turns
settle through the runtime without a persisted thread, and macOS Seatbelt uses
parameterized path rules with fail-closed enforcement.

Earlier v0.2.52 separates Goal continuation admission from
cross-turn progress detection. A turn now ends as advanced, resumably
interrupted, or blocked. (Historical note: this release used
`TurnEndReason::MaxInnerTurns`; the execution-budget redesign superseded it
with typed `OperationTerminal::Stopped` terminals.) Reaching the inner-turn
limit preserved that historical `MaxInnerTurns` reason and continued with a
structured handoff; cost-budget exhaustion, cancellation, approval, and
verification failure still paused.
Soft-landing reminders ask the model to finish its current atomic step and
update the task plan before the limit is reached.

The durable no-progress watchdog remains a separate safety boundary. It counts
completed mutating tools and structured plan changes as substantive progress,
keeps productive-turn barriers across SQLite recovery, pauses after three
repeated model-fixable gaps, and independently caps eight consecutive
budget-stop interruptions (the historical name was `MaxInnerTurns`).
The continuation envelope carries the objective,
budget state, open gap, task plan, and a bounded assistant checkpoint so a new
session can resume without repeating repository exploration.

Earlier v0.2.50 completed the runtime-owned Goal continuation model.
A per-thread `GoalActor` owns state transitions, run and outer-turn ledgers,
usage, terminal verification, recovery, and SQLite persistence. `RuntimeHost`
owns the composite Goal run, cancellation, and continuation admission. The TUI
submits commands and renders semantic events instead of scheduling turns. Goal
continuation has no fixed turn ceiling: state, cancellation, pending
interactions, workflow ownership, no-progress detection, and token budget are
the stopping boundaries. The persisted continuation counter is telemetry, not
admission policy.

Earlier v0.2.43 scopes the Linux fail-closed default to strict
restricted-read policies. Untrusted folders and strict read-only modes still
refuse to run when neither bubblewrap nor Landlock can enforce them, while
non-strict capability modes keep their established Landlock-plus-seccomp or
plain compatibility fallback when a policy needs bwrap-only features and no
bwrap is on PATH. CI now exercises that fallback path directly.

Current baseline: v0.2.42 adds OS-enforced Linux command isolation and
folder-level trust. Linux uses bubblewrap when available, with Landlock plus
seccomp as the compatible fallback; strict restricted-read policies fail closed
when neither backend can enforce them. Unknown folders load no project-local
configuration, instructions, skills, or named workflows, and start from a
read-only, no-network default without overriding an explicit permission profile
or sandbox policy.

Earlier v0.2.36 gives synchronous single and batch subagents one
runtime-owned invocation lifetime. `RuntimeToolCallRuntime` owns admission,
lifecycle start, child cancellation, worker spawn, join, panic classification,
schema validation, usage, worktree completion, result formatting, and the
exactly-once terminal. Interrupt reaches every admitted child, stops later
admission, waits for cleanup, preserves provider order, and returns the TUI to a
clean next prompt. A child panic becomes an indeterminate tool result plus a
failed subagent terminal instead of escaping RuntimeHost.

Async delegation remains a durable process task with a separate cancellation
domain. Launch now publishes `task.status.updated` immediately without an
unmatched foreground subagent row. Atomic PID adoption prevents a fast worker's
progress or terminal state from being overwritten, and foreground interrupt
does not stop work that remains explicitly cancellable through `task_stop` and
the TUI task panel. The inline single-child loop, scoped batch runtime, duplicate
formatting, stale adoption path, generic foreground child-executor plumbing,
and source-shape ownership tests are deleted. P1.2c is complete; cross-process
lease, fencing, stale-owner takeover, and task-wide publication remain P1.4.

Earlier v0.2.35 gives each sequential normal tool call one runtime-owned child
lifetime. `RuntimeToolCallRuntime` owns admission, started state, the invocation
cancel token, registered interrupt semantics, the worker, join, panic
classification, permission deltas, and exactly-once terminal. Bounded typed
bridges carry output, permission requests, and MCP elicitation without moving
borrowed TUI or server handlers into the worker. Interrupting bash, an external
tool, or an MCP call waits for process and transport cleanup before the turn
settles or the next prompt starts. P1.2b is complete.

Earlier v0.2.34 moves parallel read-only invocation lifetime into
`RuntimeToolCallRuntime`. Each admitted call has one owner for its concurrency
permit, cancellation view, started state, blocking task, join, panic
classification, and exactly-once terminal. Interrupt reaches calls that have
already started, waits for every worker and transport to clean up, and only
then publishes the RuntimeHost terminal or admits the next prompt. Results are
persisted in provider order even when execution completes out of order.

MCP resource list, template, and read operations accept typed cancellation.
The stdio transport performs a bounded reconnect before returning from a
cancelled request, while SSE closes the request and remains reusable. Direct
CLI and workflow callers without an ambient Tokio runtime use a batch-owned
runtime that is dropped after all workers join. P1.2a is complete.

Earlier v0.2.33 closes the remaining duplicate-write gap in the first P1.1
identity chain. Every submitted hosted prompt has one admission owner and one
durable user item. Foreground and host-adopted
background work share one publication boundary; sequence ranges are reserved
durably before use; selected semantic events are appended before observer or
output visibility; logical turn and conversation-item ids are allocated at
their ownership boundaries; and model replay plus public history reduce the
same canonical completion event. Tool calls and workflows retain their domain
ids, malformed current completions fail closed, and only explicitly legacy
records use isolated compatibility reducers. No current hosted path can append
the same user prompt twice, and no read-time deduplicator hides conflicting
facts. A real DeepSeek gate records, cold-reads, exits, resumes in a second
process, and verifies exact user counts plus complete prior item-object
prefixes and internal/external ids.

Earlier v0.2.30 completes the production TUI migration onto the process-owned
`RuntimeHost`. Foreground turns, DeepSeek stream interruption, interactive
waits, background providers, and saved workflows share the canonical runtime
turn and host-owned cancellation/join boundary. The duplicate TUI
provider/tool/workflow/subagent loops, local operation cancellation owner, and
`TuiTaskSupervisor` are deleted.
Earlier v0.2.29 extends the runtime ownership model into the
process-owned `RuntimeHost` and bounded `ThreadActor` control plane. Typed
operation handles and completion terminals now give headless and TUI turns one
explicit lifecycle boundary. The same release finalizes structured `@` mention
bindings across files, skills, plugins, and MCP resources, recovers rejected
submissions, isolates mention search and input history, and improves TUI
selection, clipboard, status formatting, and submission hints.
Earlier v0.2.28 removes the last production server
`CancelToken::reset()` path. `ActiveTurnManager` now owns one
`turnId -> ActiveTurnEntry` record containing the worker, generation control,
bounded resume mailbox, steer handle, join ownership, and session-permission
metadata. Interrupt permanently cancels the current generation; resume waits
for that generation to return, starts a fresh scope on the same logical turn,
and does not append the original user prompt again. Permission, user-input, and
MCP waiters are cancellation-aware and generation-fenced, stale responses and
steer input are rejected, and replaced generations cannot publish stale
`session.completed` output or runtime/outer cancellation errors. The first
generation keeps its request-id shape; resumed interaction ids carry an internal
generation suffix. CLI arguments, TUI
keys and flows, server/JSONL methods and event names, persistence, and DeepSeek
request behavior remain compatible.

Focused cancellation tests, 778 `orca-runtime` tests, and 107 `server_mode_*`
contracts pass. The full serial workspace gate, workspace Clippy, site build and
SEO, release-helper tests, and the real DeepSeek provider/CLI/history/server,
active turn-control, thread-memory, metadata, list/search, and paginated-read
gates pass. Release workflow `29398197850` passed the complete test, version
check, four-platform build, GitHub Release, npm publish, and npm release-asset
jobs. The public verifier confirmed the GitHub Release,
`@blade-ai/orca@0.2.28`, and `npm exec` installation.

P0.3a introduced a process-owned `RuntimeHost`, one bounded-mailbox
`ThreadActor` per conversation,
an owned and thread-safe `HostedTurnRequest`, and typed `OperationHandle` /
`OperationCompletion` terminals. An actor owns idle `RuntimeThread` state; one
joined operation task owns it while running and returns it before another turn
can start. Explicit interrupt is fenced by `OperationId`, concurrent start is
rejected, handle or event-subscriber loss is not reported as cancellation, and
thread/host shutdown cancels and joins active work. The operation task still
delegates to the existing `RuntimeThread -> ThreadTurnExecutor` path. At that
checkpoint no surface had migrated, so it was not a release point.

P0.3b now moves the persistent event sequence into the thread actor beside
`RuntimeThread`, gives hosted operations a typed turn-versus-headless-session
envelope, and migrates headless execution as the first production
`RuntimeHost` client. The host owns session start/end hooks and events, while a
bounded acknowledged relay preserves the existing borrowed-writer controller
API and reports downstream writer loss as typed execution failure. The
controller's direct thread, event-factory, hook, and session-terminal ownership
and the obsolete source-shape assertion protecting that path are deleted.
Focused host/controller/JSONL tests, the runtime and full serial workspace
gates, Clippy, and real DeepSeek CLI plus history-resume headless smokes pass.
This remains unreleased architecture work: it removes the lifecycle ambiguity
that blocked a safe TUI migration, but it does not yet move the TUI's production
loop onto the host.

P0.3c now makes one actor-owned `OperationId` span every generation of a
logical turn. Each executor attempt has a typed `GenerationFence`, fresh cancel
token, joined task, and generation-aware state snapshot. Interrupt, resume,
steer, and generation validation are serialized through the actor mailbox;
resume coalesces and cannot start until the cancelled generation returns its
thread, event factory, writer, and task lifecycle ownership. The actor creates
the only steer queue, reopens the same task id as Running for a resumed
generation, marks the request as an existing turn so the original user prompt
is not appended again, and publishes one operation terminal after the final
joined generation. Headless sessions remain single-generation and reject
resume. Its mandatory follow-up was to migrate server active turns without
wrapping those old owners around `RuntimeHost`. Seventeen host behavior tests,
780 runtime unit tests, 130 server contracts, 467 TUI tests, the full serial
workspace gate, workspace Clippy, and the complete real DeepSeek release
harness passed at that checkpoint.

P0.3d now migrates production server threads onto one process-owned
`RuntimeHost`. Each `ThreadActor` permanently owns its live `RuntimeThread`;
the server keeps only thread metadata plus a `turnId -> {threadId,
OperationHandle}` routing index. Submit, interrupt, resume, steer, permission,
user-input, and MCP paths use typed actor commands and `GenerationFence`
admission. The actor owns effective per-turn config, persisted task identity,
fresh generation handlers, terminal commit/drop, cancellation, and every join.
`ActiveTurnManager`, the server generation loop, resettable cancellation,
resume mailbox, detached reaper, generation writer, and take/put thread path
are deleted. Server EOF now cancels and joins actor work before returning, and
live metadata and projections remain available while a turn runs.

All 18 host behavior tests, 21 server-runtime contracts, 132 session-server
contracts, 767 runtime unit tests, 12 task-output tests, and 495 TUI tests pass.
The full serial workspace gate and workspace Clippy pass, as does the complete
real DeepSeek provider/CLI/history/server harness, including thread memory,
active-turn resume, controls, metadata, list filters, search, and turn/item
pagination. P0.3d remains unreleased foundation work: the next slice migrates
the TUI onto this proven control plane and deletes its outer operation,
provider-worker, and direct turn-loop ownership before claiming the
user-visible reliability improvement.

P0.3e1 now begins that TUI migration by making the current worker lifetime
explicit before changing session ownership. `TuiAgentRuntime` owns and joins
the agent thread, and a bounded supervisor owns background-current-turn and
auto-memory tasks. Each provider stream now keeps its receiver, cancel token,
and join handle together: foreground turns join after the terminal response,
while backgrounding transfers the complete task to the supervisor. TUI exit
disconnects the bounded event receiver and restores the terminal before it
cancels and joins runtime work. Shutdown closes operation admission before a
non-blocking wake, so a full action mailbox cannot block exit or start a late
turn. Auto-memory uses a cancellable provider path, and `TaskRegistry`
atomically makes a background stop request win over a racing provider success
while preserving incurred usage, including a provider terminal queued before
the completion consumer observes stop. Backgrounding, next-submit, foreground,
approval, goal, history, usage, and budget behavior remain compatible.

P0.3e1 is an unreleased reliability checkpoint, not the completed host
migration. The TUI still owns `OperationCancellation`, mutable
`TuiConversationSession`/`RuntimeThread`, borrowed interaction adapters, and a
surface-specific provider/tool loop. P0.3e2 must introduce the owned typed
interaction broker; later slices move the session and background handoff into
`RuntimeHost` and delete those remaining paths. The existing detached workflow
notification watchers are also still visible debt and must move behind the
host-owned event/task boundary before P0.3e is complete.

P0.3e1 verification passed with core 143/143, runtime 769/769, and TUI 506/506
tests; the serial workspace all-targets gate; workspace Clippy with only
pre-existing warnings; the release real-API smoke; and the full DeepSeek
provider/CLI/history/server control, metadata, search, and pagination harness.

P0.3e2 replaces borrowed TUI interaction handlers with an owned broker and
typed dispatcher. Approval, permission, user-input, and MCP responses are
fenced by operation id, request id, and interaction kind. Interrupt and
shutdown wake every waiter, late responses cannot reach a reused id, and the
bounded ordinary-command mailbox cannot block interaction control or runtime
teardown.

P0.3e3 moves production TUI session ownership into one `RuntimeHost` and one
actor-owned `RuntimeThread`. The UI keeps typed thread and operation handles,
while interrupt and terminal projection follow the addressed `OperationId` and
joined `OperationCompletion`. The local `OperationCancellation`, compatibility
session owner, dual hosted/local controller paths, and source-shape ownership
tests are deleted.

P0.3e4 makes `ThreadTurnExecutor` the only provider/tool/compaction/hook turn
kernel for TUI, server, and headless use. Provider suspension and background
workflow batches have typed outcomes; RuntimeHost admits, cancels, joins, and
settles background providers and workflows. Saved `/workflow` commands submit
typed `HostedWorkflowRequest` values. The old TUI agent runner, provider/tool/
workflow/subagent execution modules, task supervisor, detached workflow
watcher, and their source-shape tests are deleted. Runtime lifecycle operation
ids are also distinct from persisted turn ids and `TaskRegistry` task ids, with
a bounded server regression test covering the ownership boundary.

P0.3e final validation passes runtime-host 39/39, TUI 389/389, runtime 769/769,
the complete serial workspace all-targets gate, workspace Clippy with the
existing non-deny baseline, site and release-helper gates, the real DeepSeek
provider/CLI/history/server harness, and production PTY runs for streamed
output, interruption, next-submit recovery, usage/context projection, and
terminal restoration. P0.3e is now a releasable TUI reliability feature rather
than an unreleased compatibility stage.

P1.1a now removes the provider-background sequence fork left after P0.3e. A
host-adopted provider worker receives a fork of the actor-owned `EventFactory`
allocator, preserving one unique contiguous `(run_id, seq)` stream across the
foreground-to-background handoff without changing the event schema or wire
payloads. The RED handoff test, core event-schema tests, all 40 runtime-host
tests, the serial workspace all-targets gate, and workspace Clippy pass. This
slice deliberately stopped before workflow stream unification.

P1.1b now removes the independent workflow worker and panic-path factories.
Turn-launched workflows, saved workflows, capacity cleanup, shutdown failure,
and the concurrent next foreground turn all consume forks of the parent
thread's allocator. Workflow payload `runId` remains the workflow run id while
the envelope `run_id` consistently identifies the parent thread, preserving
the existing server/TUI payload contract. RED/green lifecycle coverage, all 41
runtime-host tests, the serial workspace all-targets gate, and workspace Clippy
pass. Ordered multi-producer publication, durable semantic journal records,
and replacement of index-derived `turn-N`/`item-N` projection ids remain the
next P1.1 vertical slices.

P1.1c now moves final sequence and timestamp assignment from event construction
into one thread-owned publication boundary. `EventFactory` produces typed
`EventDraft` values; `EventSink::emit` and the observer-only `observe_event`
path consume them while serializing observer, writer, and flush side effects.
An unpublished draft consumes no sequence, while a failed publication consumes
its assigned sequence before cleanup can continue, so no later event can reuse
it. RuntimeHost no longer calls observers directly, its concurrency assertions
verify arrival order without sorting, and the allocation-time event
`AtomicU64` is deleted. Event schema, payloads, JSONL text, server methods, TUI
flows, persistence, cancellation, and shutdown behavior remain compatible.
After rebasing onto the goal continuation redesign, 152 core tests, 43
RuntimeHost tests, 390 TUI tests, the serial workspace all-targets gate, and
workspace Clippy pass. A targeted real DeepSeek CLI and history-replay gate
also passes through the new JSONL publication path, including compatibility
repair without re-executing an incomplete legacy tool call. Durable semantic
journal records and replacement of index-derived `turn-N`/`item-N` projection
ids remain the next P1.1 vertical slices.

P1.1d now makes that thread event identity non-repeating across process
recovery. The publication boundary owns both the next sequence and an optional
typed `EventSequenceStore`; before the first event in each block of 256 it
persists an exclusive `event.sequence.reserved` high-water record under the
same lock that assigns publication order. `SessionTranscript` reduces those
records to their maximum, `RuntimeThread` restores that value only for resume,
and `ThreadActor` obtains its factory through the thread boundary. A crash may
leave a bounded gap, but the same `(run_id, seq)` cannot be reused. Fresh and
legacy histories begin at zero, forks reset because they mint a new thread id,
and rename rewrite plus zstd compression preserve the reservation. Event
schema, envelopes, payloads, CLI/TUI/server flows, and existing history records
remain compatible.

All 156 core tests, 772 runtime tests, 46 RuntimeHost integration tests, 390 TUI
tests, the serial workspace all-targets gate, and workspace Clippy with the
existing warning baseline pass. The real DeepSeek CLI/history-repair gate also
passes. A dedicated two-process smoke emitted `seq=0..47` after reserving
through `256`, then resumed the same thread id at `seq=256..287` after reserving
through `512`. P1.1d remains an unreleased reliability prerequisite. P1.1e can
now persist selected semantic lifecycle and terminal envelopes without using
token deltas as a journal; P1.1f can then replace index-derived
`turn-N`/`item-N` ids with the durable event identity.

P1.1e now makes selected semantic event identity durable before publication.
`EventPublication` remains the only owner of sequence, timestamp, publication
order, and outward visibility; its typed `EventPublicationStore` reserves the
next sequence block and appends a complete `EventEnvelope` under the same lock.
`SessionWriter` is the only durable implementation. A semantic append failure
blocks observer/output delivery and consumes the ambiguous identity, while a
reservation failure consumes nothing. Semantic events are journaled even when
there is no external observer, but transient no-observer drafts remain
unpublished. The sequence-only store is deleted, and no surface-specific or
per-delta journal exists.

The journal is a stable-identity and audit source during this migration, not a
second domain reducer. Conversation messages, task sessions, plan state, usage,
compaction, and completion records retain their existing recovery ownership.
Rewrite, rename, redaction, archive, zstd compression, restoration, fork,
legacy, and malformed-tail behavior preserve that boundary. This eliminates
the crash window where the TUI or server could observe a semantic lifecycle
event without a durable identity, while keeping disk flushes off DeepSeek token
latency.

Focused validation passes with 36 core event tests, all 161 core tests, 775
runtime tests, 48 RuntimeHost integration tests, 390 TUI tests, 14 exec JSONL
contracts, 11 history contracts, 21 server-runtime tests, 132 session-server
tests, and 14 thread-store writer tests. The serial workspace all-targets gate
and workspace Clippy pass with the existing warning baseline. The real
DeepSeek CLI/history-repair gate also passes. An isolated two-process smoke
journaled four exact semantic envelopes at `seq=0..56`, then resumed the same
thread id and journaled four at `seq=256..300`; reservations advanced from
`256` to `512`, while 51 first-process and 39 resumed-process stream deltas
produced no journal records.

P1.1f now replaces position-derived public history identity with typed logical
turn and conversation-item ownership. `HostedTurnRequest` allocates one
`TurnId` before actor admission and retains it across resumed generations;
`SessionWriter` clones share one ordered record ledger but keep independent
turn scopes, updating the ledger only after the JSONL append succeeds. New
`conversation.message` records persist both `id` and `turn_id`, and
`SessionTranscript` exposes identified records as the single projection source
while retaining messages as a derived model-replay view.

`ThreadStore` groups identified records by their persisted turn id. Ordinary
messages use their persisted item id; tool calls keep the DeepSeek call id,
file changes add the existing `:file-change` suffix, workflows keep their run
id, and tool results update the request item instead of minting another public
wrapper. Malformed partial identity fails closed. One named legacy projector
keeps old histories readable, but no current writer, RuntimeHost path, or
server route can mint `turn-N` / `item-N` identity.

Focused core, RuntimeHost, server-runtime, ThreadStore, persistence, TUI, and
compatibility tests pass, followed by the serial workspace all-targets gate and
workspace Clippy with the existing warning baseline. The real DeepSeek release
harness records and cold-reads one turn, starts a second `orca exec` process,
resumes the same thread, recalls the first-process sentinel, and proves that the
old turn/item id prefix is unchanged while the resumed turn receives new typed
ids. P1.1f is complete and released in v0.2.31.

P1.1g now makes completed model output canonical across live and cold paths.
`ModelResponseIdentity` reserves agent-message, reasoning, and proposed-plan
ids before the first provider delta and travels with the response through
suspension, host adoption, persisted approval state, and continuation. After
runtime validation, one typed `CompletedModelResponse` is published and
persisted as `model.response.completed`; new assistant responses are no longer
double-written as `conversation.message` records.

`SessionWriter`, `SessionTranscript`, `JsonlThreadStore`, and
`RuntimeEventProjector` reduce that same fact. Live `item_started`, delta, and
`item_completed` events use the ids that cold `thread/turns/list` and
`thread/items/list` return after restart. The projector no longer owns static
text ids, completed-text accumulation, or terminal completion synthesis. The
combined assistant projection remains only in the named legacy-record branch,
and malformed canonical events cannot fall back to it.

Focused core, runtime, RuntimeHost, TUI, session-server, server-runtime,
ThreadStore, JSONL, and history tests pass, followed by the serial workspace
all-targets gate and workspace Clippy with the existing warning baseline. The
real DeepSeek release gate verifies CLI output, legacy repair without tool
re-execution, cross-process record/reload/resume, complete canonical item-object
prefixes, matching internal/external ids, server thread memory, active-turn
resume, and paginated turn/item projections. P1.1g is complete and released in
v0.2.32.

P1.1h now makes user-turn admission canonical. `ThreadTurnContext::prepare`
remains the sole hosted owner: it binds the logical turn, appends one identified
`conversation.message` record, and only then commits the prompt to the model
conversation. A failed durable append therefore prevents an unrecorded prompt
from reaching DeepSeek or the public projection ledger.

`AgentConversationContext` is now an explicit `Owned` / `Borrowed` provenance
boundary. Owned child-agent bootstrap creates independent model context and has
no session writer. Borrowed bootstrap uses the runtime session's conversation
and writer for later tool and completed-model records, but never bootstraps the
already-admitted user prompt. The obsolete initial-history helper and its
source-shape ownership tests are deleted; no read-time duplicate filter or
compatibility branch replaces them.

Focused runtime, RuntimeHost, server, ThreadStore, real-harness, and persistence
tests prove one user item per logical turn, transactional admission failure,
and matching live/cold identities. The serial workspace all-targets gate,
workspace Clippy with the existing warning baseline, site and release-helper
gates, and the real DeepSeek cross-process server harness pass. Existing
duplicate histories remain readable without mutation. P1.1h is complete and
released in v0.2.33.

P1.2a now gives parallel read-only tool calls one runtime-owned lifecycle.
`RuntimeToolCallRuntime` owns admission permits, cancellation observation,
execution-start state, blocking workers, joins, panic classification, and the
exactly-once terminal for every invocation. RuntimeHost interrupt therefore
reaches already-started calls and waits for all cleanup before publishing the
operation terminal or admitting the next turn. Natural completion wins a
cancellation race, a panic after dispatch is indeterminate, and batch results
retain provider request order even when workers finish out of order.

MCP resource list, template, and read paths now carry typed cancellation through
the registry, client, and transport. Cancelled stdio requests perform a bounded
reconnect before returning, while SSE requests close and leave the transport
reusable. Direct CLI and workflow child-agent paths without an ambient Tokio
runtime create a batch-owned runtime whose lifetime ends only after every
worker joins. The `orca-tools` scoped-thread batch helpers, their pre-spawn-only
cancellation test, the top-level shim export, and source-shape ownership tests
are deleted.

Runtime, RuntimeHost, MCP, tools, workflow, and TUI behavior tests pass, followed
by the serial workspace all-targets gate, workspace Clippy with the established
warning baseline, site and release-helper gates, and the full real DeepSeek
provider/CLI/history/server harness. CLI arguments, TUI flows, server/JSONL,
persistence, and provider request ordering remain compatible. P1.2a is complete
and released in v0.2.34; normal tools and subagents remain P1.2b and P1.2c.

P1.2b now gives every sequential normal tool call one runtime-owned child
lifetime. `RuntimeToolCallRuntime` owns pre-start admission, the started marker,
registered `CooperativeCancel` or `WaitForTerminal` behavior, a child cancel
token, the blocking worker, join, panic classification, and exactly-once
terminal selection. Output chunks, permission requests, and MCP elicitation use
bounded typed bridges; permission changes return as a typed overlay delta and
merge into canonical turn state before later sibling calls.

Interrupting bash, external tools, or MCP calls now waits for process,
managed-proxy, and transport cleanup before the RuntimeHost terminal is
published or another turn starts. `WaitForTerminal` keeps the observed result,
natural completion wins a cancellation race, a panic after start is
indeterminate, and output publication failure cannot detach the invocation or
replace an already observed tool result. Parent callback panics are isolated so
worker ownership remains joined during unwind.

The borrowed normal execution context, fallback executor owner, direct inline
router path, and associated source-shape tests are deleted. Runtime-special
tools, workflows, and subagents retain their explicit owners rather than being
wrapped in a second scheduler. Sixteen normal-runtime ownership tests, all 794
runtime library tests, 59 lifecycle contracts, four server interrupt
regressions, the serial workspace gate, workspace Clippy, site and release
helpers, and the complete real DeepSeek provider/CLI/history/server harness
pass. CLI arguments, TUI flows, server/JSONL, persistence, tool identities, and
permission payloads remain compatible. P1.2b is complete and released in
v0.2.35.

P1.2c now gives synchronous single and batch subagents one runtime-owned
invocation lifetime. `RuntimeToolCallRuntime` owns admission, lifecycle start,
the invocation-scoped child cancel token, worker spawn, join, panic
classification, schema validation, usage, worktree completion, provider-order
result folding, and exactly-once terminal selection. Interrupt reaches all
admitted children, prevents later admission, waits for every worker and
worktree owner to finish, and returns RuntimeHost to a clean next prompt. A
child panic after admission becomes one indeterminate tool result and one
failed subagent lifecycle terminal without escaping the host; clean worktree
isolation is still removed after the panic.

Async delegation remains an independently cancellable durable process task.
Launch emits the existing typed `task.status.updated` event for immediate TUI
projection and no longer emits an unmatched foreground `subagent.started`.
`TaskRegistry` atomically adopts the real PID, Running state, and start time
before the worker may persist progress or terminal state. Late adoption cannot
overwrite a terminal task, the parent reaper reloads cross-process completion,
and foreground interrupt does not stop a registered async worker; `task_stop`
and the TUI task panel remain the explicit cancellation owners.

The inline single-child loop, `thread::scope` batch runtime, duplicate
schema/worktree/result formatting, stale parent adoption path, obsolete generic
foreground child-executor plumbing, and source-shape ownership tests are
deleted. All 800 runtime library tests, 50 RuntimeHost integration tests, 12
JSONL subagent contracts, focused TUI projections, the serial workspace gate,
workspace Clippy with the established warning baseline, site and release helper
gates, and the complete real DeepSeek release harness pass. A dedicated real
DeepSeek subagent smoke returns successful child and parent sentinels with
paired lifecycle events and a successful session terminal. CLI, TUI,
server/JSONL, persisted task fields, tool schemas, synchronous subagent payloads,
provider order, and DeepSeek request behavior remain compatible. P1.2c is
complete and released in v0.2.36; cross-process live publication, lease,
fencing, and stale-owner takeover remain P1.4.

Earlier v0.2.26 replaces the TUI's unbounded runtime-event and
user-action lanes with blocking bounded mailboxes of 256 and 64 values. Slow or
paused rendering now applies producer backpressure without silently dropping
assistant output, approval, error, or terminal events. Runtime compaction and
approved background continuation project the original typed `EventEnvelope`
through `EventObserver`; the TUI-local JSONL writer, partial-frame buffer, and
deserialize/recreate envelope path are deleted. Provider streaming, mention
catalog refresh, and silent child-agent event disposal also have explicit
bounded admission, and the silent drain worker is joined before return. CLI,
TUI interactions, server/JSONL, persistence, and DeepSeek behavior remain
compatible. The release passed focused observer/mailbox/compaction, complete
serial workspace, Clippy, site, release-script, and real DeepSeek provider
gates. Release workflow `29388934712` passed the complete test, four-platform
build, GitHub Release, npm publish, and npm release-asset jobs; the public
verifier confirmed the GitHub Release, `@blade-ai/orca@0.2.26`, and `npm exec`
installation.

Earlier v0.2.25 gives the managed network proxy one explicit
supervisor owner. It admits at most 32 concurrent connections, bounds request
and header framing before parsing, sends permission-block reports through a
fixed-capacity channel, resolves and connects with deadlines, and keeps every
connection in one cancellable `JoinSet`. Dropping a TUI bash, runtime bash, or
server `command/exec` proxy stops admission, aborts and awaits all active
connections, closes CONNECT tunnel endpoints, and joins the supervisor thread.
CLI, TUI, permission-profile, proxy environment, server/JSONL, and persistence
contracts remain unchanged; overload and framing violations now receive
bounded HTTP diagnostics. The release passed the complete serial workspace,
Clippy, site, release-script, and real DeepSeek provider gates. Release workflow
`29385203632` then passed the complete test, four-platform build, GitHub
Release, npm publish, and npm release-asset graph; the public verifier confirmed
the GitHub Release, `@blade-ai/orca@0.2.25`, and `npm exec` installation.

Earlier v0.2.24 closes every accepted tool call with one truthful terminal,
repairs incomplete historical calls without replay, bounds process output and
tool-facing file admission, and gives ordinary tools, MCP servers, workflows,
async subagents, verifier commands, and server shells an explicit cleanup or
reaper owner. Recovered workers are identity-checked before signaling, server
shutdown is bounded and two-phase, and internal worker API keys use a bounded
anonymous stdin pipe instead of argv or workflow records. Known follow-up areas
include Windows descendant-tree parity, a total WorkflowHost deadline, and
runtime-owned file-input admission.

Earlier v0.2.23 gives the TUI native-feeling mouse text
interaction. Left-drag selects transcript text with a theme-aware,
foreground-preserving highlight; release copies through OSC 52 (plus
`pbcopy` on macOS) with a transient status-line notice. Selections anchor in
content space so streaming and scrolling never shift them; extraction
restores whitespace dropped at soft wraps while keeping hard-split words
unbroken, and counts display width for CJK. Double-click selects and copies
the word under the cursor, dragging onto the first/last transcript row keeps
auto-scrolling from the animation tick while the pointer sits still, and a
floating `Jump to bottom` pill re-arms auto-follow on click.

Earlier v0.2.22 replaces the synchronous TUI-only `@file` index with
an owned streaming `orca-file-search` subsystem. It supports canonical
multi-root browse/fuzzy search, exclude and Git-ignore controls, million-path
bounded latency/memory gates, Codex-compatible app-server `fuzzyFileSearch/*`
sessions, and thread-bound `mention/search/*` sessions. Files, Skills,
Plugins, MCP Resources, and Resource Templates now share typed candidates and
stable target identities. TUI and app-server submissions carry atomic
bindings, rebase them across preceding edits, invalidate them on overlapping
edits, and revalidate/expand the exact selected target through the active
workspace and MCP registry. Legacy `@file` and `$skill` input remains
compatible.

Earlier v0.2.21 gives a DeepSeek turn that ends without visible content or a
tool call one bounded semantic recovery request. A request-local
instruction changes the retry without mutating conversation history, preserves
the preceding valid tool-call/tool-result boundary, and keeps reasoning-only
responses invalid. Streaming recovery suppresses reasoning already shown by the
failed attempt while continuing to emit recovered content and tool progress.
Reported usage from both attempts is combined. Foreground and detached TUI turns
no longer lose usage reported by the failed recovery attempt, while controller
paths retain their runtime-owned accounting. Foreground TUI provider requests
use serialized admission while the historical `max_budget_usd` config was
active (superseded by the `[budget]` execution-budget redesign), and later
turns failed preflight before their prompt entered conversation history once
completed usage was over budget. Synchronous child-agent usage is emitted and persisted
immediately; budget mode disables parallel batches and rejects asynchronous
children so their usage is reconciled before the parent turn proceeds. Each
synchronous child receives only the aggregate budget remaining in its parent,
and crossing that limit prevents later children in the same provider turn.
Detached provider completions remain task-scoped: priced usage and redacted
errors are persisted as `background_task.provider_response`, without writing a
global `session.completed` record. A `session.usage_baseline` starts a new
accounting epoch on resume, preserving earlier foreground snapshots and
detached deltas without loss or double counting.
Foreground and background TUI failures and controller-backed CLI/server turns
keep the real diagnostic in task and session state; optional
`session.completed.error`, detached `background_task.provider_response`, and
persisted task errors use the existing secret redactor before disk writes.
Goal usage now counts input plus output tokens rather than adding cache hits a
second time. Detached completions and approved continuations apply exact usage
deltas, while synchronized Goal-store mutations prevent concurrent updates from
overwriting each other. Historical cache-inflated Goal values are not migrated
automatically.
Earlier v0.2.20 makes the TUI
resilient to long interactive content: large pastes remain compact before and
after submission, Goal and plan surfaces truncate by display width, approval
decisions stay visible, candidate menus follow selection, tool headers preserve
status, and the footer degrades by information priority with permission-mode
semantic colors. Earlier v0.2.19 lets process managers inside the macOS workspace-write
and read-only Seatbelt profiles signal their own child workers. This restores
Vitest, Tinypool, Jest, and similar worker-pool cleanup after failures and
shutdown without granting authority over unrelated or same-sandbox processes.
The regression suite exercises real child termination in both profiles and
keeps the broader signal targets denied. Earlier v0.2.18 preserves unknown
DeepSeek function calls instead of turning them into terminal provider errors.
Each call keeps its provider id,
function name, and raw arguments. Configured external names remain
`ToolName::External` with their declared action, unresolved `mcp__*` names
remain `ToolName::Mcp`, and other generic unresolved names become
`ToolName::External`. Every unresolved request receives provisional
`ActionKind::Read` and fails registry validation before approval, hooks, task
creation, or execution. Orca records the matching failed tool result and
returns it to DeepSeek for correction inside the same agent turn;
command-shaped names are never inferred or executed as `bash`. Streaming and
non-streaming responses use the same preservation boundary. Earlier v0.2.17
keeps the active Goal activity timer
cumulative across automatic continuations by rendering persisted
completed-turn time plus the current-turn delta. Between-turn, paused,
terminal-status, and offline time remains excluded. `/goal resume` preserves
the objective, token budget, token usage, elapsed time, and original creation
timestamp instead of replacing the Goal record. Same-session reactivation and
cross-session migration use one persisted store replacement; cross-session
migration pauses the source, refuses to overwrite an occupied target, and
leaves both persisted and live TUI state unchanged on failure. Restored history
projects the preserved Goal state before `TurnStarted`, so the first running
frame includes its elapsed-time base. This remains a TUI-owned continuation
path; the runtime Goal orchestrator, durable cursor, lease, and fencing work are
still open. Earlier v0.2.16 makes context compaction visible through its full
TUI lifecycle. Automatic soft-limit, hard-limit, and
prompt-too-long recovery emit typed `context.compaction.started` before budget
hooks, compaction hooks, or remote summary work can block. Manual `/compact`
also enters the existing `Compacting context...` state before its synchronous
summary call. Ctrl+C, Esc, and Ctrl+G remain live in that state and now cancel
hooks plus the underlying streaming DeepSeek HTTP operation. Waiting for
response headers, retry waits, error bodies, and SSE reads now race
cancellation through async reqwest with operation-scoped Hickory-DNS clients.
The temporary synchronous provider facade uses one joined worker and an
acknowledged zero-capacity step handoff, so callback cancellation cannot leave
prefetched or same-frame deltas, or detached transport work, behind. Malformed
or prematurely ended SSE streams fail explicitly and retry only before any
visible step; a known tool with malformed JSON arguments is preserved as a
tool request, then validated before approval, hooks, task creation, or
execution so Orca can return a corrective tool failure. Successful compaction
is projected only after summary persistence and the post-compact hook finish,
while retaining the detailed reason, strategy, and collapsed-message notice.
Earlier v0.2.15 makes DeepSeek history replay
reject incomplete assistant turns. TUI resume removes legacy assistant turns
that contain reasoning but no visible content or tool calls before the next
provider request, and new reasoning-only responses are retried instead of
being persisted. Valid reasoning attached to tool calls remains available for
DeepSeek replay. The real API release gate now seeds malformed history and
verifies that the resumed turn completes successfully.
Earlier v0.2.14 gives server-mode clients MCP
elicitation parity with the TUI path. Stdio MCP `elicitation/create` requests
now emit `mcp_elicitation_request`, clients respond with
`mcp_elicitation/respond` by stable runtime `requestId`, and the original MCP
tool call continues after Orca writes accept/decline back to the MCP server.
The same baseline also keeps server `command/exec` ownership intact when
clients inspect shells: `shell/list` no longer reaps or lists command/exec-owned
backing shell sessions, so `command_exec_completed` remains on the command/exec
task-control path.
Earlier v0.2.13 routes runtime task output through a bounded, UTF-8-safe
task-output store. Long-running TUI bash and server `command/exec` sessions no
longer retain unbounded per-process stdout/stderr buffers, streaming
command/exec deltas survive retained-output rebasing without resetting
cumulative output caps, and completed, stopped, or permission-denied processes
evict retained output when the runtime observes the terminal path. Earlier
v0.2.12 overhauls TUI scroll performance so long sessions stay
responsive. A new frame scheduler coalesces wheel events and caps per-batch
event processing, message rendering flows through a message-version-based cache
instead of redrawing the full transcript every frame, and a virtual viewport
renders only the visible messages. Scroll metrics widen to `usize` so sessions
longer than 65,535 lines scroll correctly, and the bottom status line drops the
`scroll: N/total` indicator. Earlier v0.2.11 routes
TUI keyboard handling
through a context-aware shortcut resolver. Global, composer, running-turn, and
approval-dialog shortcuts keep their existing behavior, but the resolver,
focused tests, and shortcut help rendering now share one binding boundary so
future keymap, permission-dialog, and task-control changes can be verified
without scattering more state-specific key checks. Earlier v0.2.10 makes TUI
compacted-context notices explain the runtime compaction reason and strategy.
Runtime `context.compacted` events now project their reason, strategy,
collapsed-message count, and status text into the TUI so long DeepSeek sessions
can distinguish near-limit compaction, hard-limit compaction, and
prompt-too-long recovery instead of showing only a generic before/after message
total. Earlier v0.2.9 makes TUI main-agent automatic compaction and
prompt-too-long retry recovery runtime-owned. The TUI main loop now asks
`orca-runtime` to run pre-turn compaction and classify provider context-length
errors, while runtime `context.compacted` events still project into the TUI
compacted-context notice and context meter. This keeps the most visible
long-session recovery path aligned with server and child-agent compaction policy
instead of leaving retry state in the renderer loop. Earlier v0.2.8 moves
command/exec sandbox-policy mapping, active permission-profile
inheritance, filesystem root/glob materialization, network domain policy, and
Unix socket allowlist resolution behind a focused `server/command_exec_sandbox`
boundary. TUI bash and server `command/exec` still share the same sandbox
behavior and JSON wire shapes, but future permission, network, filesystem, and
task-control work can test that boundary without threading more policy logic
through the large server loop. Earlier v0.2.7 moves the reusable user,
persisted, assistant-message,
proposed-plan, and reasoning thread-item projection types into `orca-core`.
Runtime projection still emits the same server/TUI JSON, but live TUI streams,
active steer user messages, and resumed history/read/list views now pass
through one typed item boundary before serialization, reducing drift between
the transcript cards users see in fresh and resumed sessions. Earlier v0.2.6 made DeepSeek proposed-plan streams
visible in the TUI as a dedicated scrollback message instead of leaking
`<proposed_plan>` tags into ordinary assistant text. The server projection and
TUI share the same UTF-8-safe proposed-plan stream parser, so split tags,
incomplete plan tags, and Chinese streaming text keep one tested behavior
across local and server-facing transcript surfaces. Earlier v0.2.5 made server
`command/exec` network-deny handling use the same runtime permission evaluation
boundary as TUI bash. Requestable command/exec network blocks still produce the
existing permission request and retry flow, while configured denylist blocks
now surface an explicit policy-denial error instead of falling through as an
unpromptable missing request. Earlier v0.2.4 made TUI bash network-deny
handling use that runtime permission evaluation path, so denylisted network
blocks produce an explicit policy denial instead of disappearing as a
non-promptable `Option::None` case. Earlier v0.2.3 routed stdio MCP
`elicitation/create`
requests through the TUI pending-interaction path. The
post-v0.2.3 P0 refactor line now has several narrow slices: the runtime tool
scheduler makes `runtime_tool_scheduler` own normal, readonly batch, and
sync-subagent batch selection rules so mixed tool batches stop at
non-concurrent boundaries, and the text item projection slice moves
agent-message, plan, and reasoning item lifecycle state into
`tool_item_projection` so TUI/server streams share one tested start/delta/finish
shape for the most visible transcript items. The follow-on typed text item slice
keeps those same wire shapes but constructs agent-message, plan, and reasoning
items through a focused typed projection enum first, giving the later
Codex-style `ThreadItem` protocol migration a narrow tested entry point. The
same typed-protocol path now covers command execution plus MCP, dynamic
tool-call, file-change, and workflow transcript items too, so TUI command,
external-tool, edit/file-change, and workflow cards keep their existing wire
shapes while moving toward typed construction before shell streaming,
tool-error diagnostics, and task-control behavior evolve further. Those typed
transcript projections now also share a thin `ProjectedThreadItem` serialization
exit, matching Codex's enum-shaped item boundary without changing the current
server/TUI JSON contract. Realtime command, MCP, and dynamic tool-call
lifecycle state now also lives behind `ProjectedToolCallItem`, so the runtime
event projector stores typed projection state instead of ad hoc tool item
fields while preserving the same TUI card payloads. Realtime `fileChange`
lifecycle state now follows the same pattern through `ProjectedFileChangeItem`,
keeping edit/write cards typed before the projector emits the unchanged
server/TUI payload. Workflow lifecycle aggregation now also lives behind
`ProjectedWorkflowItem`, so run/task/result state is typed before the projector
emits the unchanged workflow card payload. Server active-steer user-message
item payloads now also use the shared `ProjectedThreadItem` path instead of
processor-local JSON, keeping TUI-visible steer cards aligned with the same
typed projection boundary. Persisted thread-store system, user, assistant, and
tool message projections now also enter that shared typed projection path
before serializing the unchanged history/read/list JSON, reducing drift between
live TUI transcript cards and resumed thread views. A server can now ask for
URL/form input during an MCP tool call, the TUI projects that request as a
visible waiting-input prompt keyed by the runtime interaction id, and Orca
writes the accept/decline response back before continuing the original tool
call. Earlier v0.2.2 hardened
DeepSeek tool-call compatibility:
`update_goal` and `update_plan` normalize the status aliases and boolean status
flags DeepSeek emits before validation, the `glob`/`update_goal` JSON Schemas
gain nullable/`anyOf` support, tool validation errors now list the allowed and
required properties, and the system prompt stops inlining full tool schemas in
favor of concise usage examples. The same line adds reasoning-content replay, a
tool-count upper bound, and an empty-response retry for DeepSeek turns, a
stale-plan freshness reminder, and a changelog-page SEO check that validates the
React source instead of rendered HTML.
Earlier v0.2.1 continued the Codex/package 3
processor pass: `permission/respond` and `user_input/respond` now resolve their
pending request ids inside focused server processors instead of the generic
`server.rs` module. This keeps server-mode interactive responses aligned with
Codex's request-processor ownership and package 3's request-id-driven pending
permission flow, without changing wire event names or response semantics.
Earlier v0.2.0 started the feature-release line with a TUI-visible permission
approval improvement: runtime pending interaction records now preserve the
permission request kind for network blocks, filesystem write grants, and
unsandboxed shell retries, and the TUI approval modal uses that kind to show a
risk-specific title instead of a generic approval title. The same v0.2.0 line
also fixes the `request_permissions` runtime path so the tool bypasses the
normal tool approval gate and reaches the runtime permission handler, letting
TUI/server users approve session-scoped directory and network grants through the
intended pending-permission flow.
Earlier v0.1.191 and the follow-on TUI bridge slice own
approved background provider-response continuation execution in `orca-runtime`:
the runtime consumes the pending provider response from `TaskRegistry` and
derives the single preapproved tool-call id before the TUI resumes a
backgrounded turn. Runtime provider-cycle, turn-loop, and agent-loop inputs
carry a typed `RuntimeTurnContinuation` instead of a bare
`ProviderResponse`; the provider cycle consumes the continuation once, seeds the
turn permission overlay with the preapproved tool-call id, and the approval gate
consumes that id exactly once for the matching tool call. The TUI bridge now
converts approved background continuations into runtime `ThreadTurnRequest`
continuations and projects runtime JSONL events back into TUI events, retiring
the renderer-owned preapproved provider/tool loop. The follow-on TUI
notification slice now preserves workflow terminal notification ids across the
pending queue, action channel, and turn-result continuation boundary instead of
recasting queued workflow continuations as plain user prompts. The TUI agent
loop now also routes submitted turns through a named source boundary: human
submits still get user-authored `@file` mention expansion, while workflow
notification continuations are forwarded as typed follow-ups without user
prompt preprocessing; the same source boundary now also supplies the TUI task
label, so workflow follow-up turns show a stable notification label instead of
raw notification payload text, uses that label as the session title seed when a
workflow follow-up creates the first recorded history thread, and records
workflow follow-ups as non-backtrackable context so the TUI backtrack command
still targets the user's last real submit. The TUI goal-turn loop now also
receives that submitted-turn boundary directly, so task-label and backtrack
presentation metadata stay grouped with the submitted turn instead of crossing
the loop as parallel ad hoc fields. Earlier v0.1.191 makes
`RuntimeProviderResponseStep` consume the
named `RuntimeProviderResponseInput` directly and carries child-agent executors
through `RuntimeProviderResponseExecutors`. Provider final-message handling and
tool-turn dispatch now keep one response handoff instead of re-expanding the
kernel-assembled response input into a long argument list. Earlier v0.1.190
carries provider-turn execution through a named `RuntimeProviderTurnInput` and
groups provider-call I/O refs behind `RuntimeProviderTurnIo`.
`RuntimeProviderTurnStep` now receives actor, provider, runtime system messages,
hook/cancel refs, budget policy, steering handle, and the grouped
conversation/history/event/sink/cost refs as one call-boundary object, while
provider behavior remains unchanged. Earlier v0.1.189 carries provider-response
I/O refs through a named `RuntimeProviderResponseIo` bundle.
`RuntimeTurnKernel` now assembles events, sink, conversation, history writer,
cost tracker, and background workflow refs as one response handoff, while
provider response handling destructures that bundle only at the execution
boundary before dispatching final-message or tool turn work. This continues the
same direction seen in Codex turn/step contexts and Claude Code query/tool
contexts: wide execution state is handed across named runtime boundaries rather
than expanded at every call site. Earlier v0.1.188 carries provider-cycle capability refs through the
same `RuntimeStepCapabilitySnapshot` used by step context. `RuntimeProviderCycleInput`
keeps provider-cycle execution fields separate from the request capability
bundle, `runtime_turn_iteration` assembles that bundle once from turn input, and
`provider_turn` passes it into `RuntimeStepContext` without expanding the flat
instructions, memory, MCP, hook, cancellation, task, workflow IPC, or interaction
handler refs. Earlier v0.1.187 moved request-scoped capability refs inside a named
`RuntimeStepCapabilitySnapshot`. `RuntimeStepSnapshot` now keeps the immediate
request execution fields while routing instructions, memory, MCP registry,
hooks, cancellation, task registry, workflow IPC, and turn interaction handlers
through that capability bundle; `tool_turn` consumes the bundle through
`RuntimeStepSnapshot::capabilities()` before dispatching readonly, subagent, and
normal tool turns. Earlier v0.1.186 routes `request_user_input` through the same
turn-scoped interaction boundary as permission requests. `RuntimeTurnInteractionState`
now carries both the permission handler and the runtime user-input handler,
`ThreadTurnRequest` can install a user-input handler for a turn, and
`runtime_turn_iteration`, `RuntimeStepSnapshot`, `ToolExecutionContext`, and
`RuntimeToolRouter` pass that handler to the point where `request_user_input`
is dispatched as a runtime special tool instead of a normal-tool fallback.
Earlier v0.1.185 grouped turn-scoped interaction handlers behind a named
`RuntimeTurnInteractionState`. `AgentLoopContext`, `runtime_turn_loop`, and
`runtime_turn_iteration` carry the permission-request handler through that
grouped turn interaction boundary before provider/tool dispatch needs it,
leaving the existing approval and permission behavior unchanged while making
room for later elicitation and dynamic-tool waiters to share the same
turn-owned interaction surface. Earlier v0.1.184 gave provider and
tool-dispatch steps a named request-scoped runtime snapshot.
`RuntimeStepSnapshot` now owns the stable per-request runtime inputs that had
been spread across `RuntimeStepContext`, while `RuntimeStepContext` carries that
snapshot plus the kernel-bound extension context. Provider final-response
handling reads settings through the snapshot, and tool dispatch splits the step
context into snapshot plus extension binding before routing normal, readonly,
workflow, and subagent tool turns. Earlier v0.1.183 gave runtime capability
changes a named snapshot contract.
`RuntimeCapabilityPatch` and `RuntimeCapabilitySnapshot` own model overrides,
allowed-tool replacements, runtime system-message injection, and transition
reasons behind directive state, while `RuntimeDirectiveState` applies patches
and exposes that shared snapshot for future skill, hook, MCP, and tool-policy
paths. Earlier v0.1.182 moved turn-loop state assembly onto a
`RuntimeTurnKernel` instance. `RuntimeTurnState` creates the kernel from the
thread and turn extension stores, then asks that instance to assemble
`RuntimeTurnLoopState`; the loop state keeps shared scoped extension stores so
the kernel can borrow the same state it hands forward. Earlier v0.1.181 let
`RuntimeTurnKernel` assemble the lifecycle-owned `RuntimeTurnLoopState` that
carries directive state, mutable runtime refs, and scoped extension state into
the turn loop. `RuntimeTurnState` no longer expands loop runtime and
extension-state fields itself, preserving behavior while moving the Codex-style
turn-state handoff through the named kernel boundary. Earlier v0.1.180 let
`RuntimeTurnKernel` assemble the
provider-response input object that carries the bound `RuntimeStepContext`,
kernel-owned sampling state, event/sink refs, conversation/history refs, cost
tracker, and background workflow handles. Provider response handling no longer
exposes kernel-owned sampling state or step-context binding as separate fields,
preserving behavior while tightening the Codex-style turn-state handoff. Earlier
v0.1.179 let `RuntimeTurnKernel` retain the runtime extension stores used by its
reducer and bind provider-response `RuntimeStepContext` extensions through the
same kernel. Provider response handling no longer wires step-context extension
stores directly, preserving behavior while tightening the Codex-style
turn-state boundary around sampling state, reducer state, and extension context.
Earlier v0.1.178 introduced a `RuntimeTurnKernel` that owns the per-sampling
request state together with the runtime turn reducer. Provider response handling
now constructs tool-dispatch state through that kernel before passing it into
tool turns, preserving behavior while giving the next Codex-style turn state
consolidation a named runtime boundary. Earlier v0.1.177
enriches server-mode `command/exec/list` snapshots with the backing `shellId`,
`taskId`, requested terminal mode, and effective terminal mode, so reconnecting
app-server clients can recover the same task identity and PTY/pipe semantics
exposed by `shell/list`. Earlier v0.1.176 added
server-mode `command/exec/list` so app-server clients can recover active
`command/exec` process handles by listing `processId`, original command argv,
`cwd`, running status, stream-output settings, output cap, and stdout/stderr
sent-byte counters; completed processes are drained before the next list
response and disappear from the active snapshot. Earlier v0.1.175 let
server-mode `command/exec/read` requests apply an `outputBytesCap` byte budget
to active streaming `command/exec` processes, tightening the process output cap
before the server's normal pre-dispatch drain and returning UTF-8-safe
`command_exec_output_delta` events with `capReached` metadata. Earlier v0.1.174 added server-mode
`command/exec/read` so app-server clients can actively drain long-running
streaming `command/exec` process handles by `processId`, receive a
`command_exec_read` acknowledgment, and reuse the existing
`command_exec_output_delta` / `command_exec_completed` stream. Earlier v0.1.173 let server-mode `shell/read` requests apply an
`outputBytesCap` byte budget to incremental shell stdout/stderr, returning
truncated UTF-8-safe deltas plus `capReached` metadata on
`shell_output_delta`, `shell_updated`, and `shell_completed` events. Earlier
v0.1.172 exposed a server-mode `shell/capabilities` operation so app-server
clients can query the current platform, native PTY and PTY resize availability,
accepted terminal modes, pipe fallback behavior, and the `processId`
requirement for streaming `command/exec` sessions before launching terminal
work. Earlier v0.1.171 fixed two sandbox/task-state rough edges:
pathless macOS sandbox denials such as GitHub HTTPS credential prompts can now
escalate through runtime, JSONL `command/exec`, and TUI approval flows to
re-run the command without the filesystem sandbox, while shell task session
state now lives under `ORCA_HOME/task-sessions` (or `~/.orca/task-sessions`)
with migration from legacy project `.orca/task-sessions` directories. Earlier v0.1.170 let
`RuntimeSamplingRequestState` record normal tool results and own the
approval-required plus subagent-failure terminal folding for single-tool turns.
Normal tool execution now borrows its permission overlay and records its result
through the same request state, leaving `tool_turn` to delegate the
per-sampling state boundary. Earlier v0.1.169 let
`RuntimeSamplingRequestState` produce clamped `RuntimeToolDispatchWindow` values
for readonly and subagent batch dispatch. Tool turns no longer read raw cursor
positions or slice batch windows directly, and the dispatch-window API
guarantees forward progress over the current request even if a batch collector
returns the current cursor. Earlier v0.1.168 let
`RuntimeSamplingRequestState` own the tool-dispatch cursor as well as the
per-sampling permission overlay. Tool turns now read and advance the current
request through sampling state instead of keeping a separate `ToolRequestCursor`,
so the Codex-style request-scoped runtime state boundary has one clearer owner.
Earlier v0.1.167 introduced
`RuntimeSamplingRequestState` as the first per-sampling request-state home and
routes normal tool turns through its permission overlay instead of allocating
local permission state inside `tool_turn`. Provider response handling now
creates that sampling state before tool dispatch, giving later Codex-style
request snapshots a concrete runtime boundary. Earlier v0.1.166 moved direct
`RuntimeTurnLoopInput` construction out of `agent_loop` and behind the focused
`run_agent_turn_loop` entrypoint. `agent_loop` passes a
`RuntimeAgentTurnLoopInput` launch object while `runtime_turn_loop` owns the
internal wide handoff to the iteration boundary. Earlier v0.1.165 let
`RuntimeTurnLoopState` own the directive-resolved loop policy surface:
`agent_loop` no longer destructures loop state or reads directive accessors
directly; lifecycle resolves tool policy, runtime system messages, model
override, cost/cancel/task refs, and grouped extension context for each
turn-loop iteration. Earlier v0.1.164 let
`RuntimeTurnState` hand `agent_loop` a lifecycle-owned `RuntimeTurnLoopState`
and moved extension context derivation to the iteration boundary, v0.1.163 moved
grouped runtime extension-context composition into the state boundary, v0.1.162 moved grouped
runtime extension routing up to the turn-loop, turn-iteration, and provider-cycle inputs,
v0.1.161 moved the grouped context into `RuntimeStepContext` and
`RuntimeNormalToolTurnContext`, v0.1.160 moved grouped extension-store routing up
to `ToolExecutionContext`, v0.1.159 grouped permission-sensitive turn/thread
extension references behind `RuntimeExtensionStores`, v0.1.158 made permission
reduction consistently instance-owned by `RuntimeTurnReducer`, v0.1.157 routed
permission overlay mutation through the reducer, v0.1.156 routed runtime
directive application through the reducer, and v0.1.155 introduced the reducer
for completed-tool goal progress.

---

## Current State

Orca has moved beyond the original MVP roadmap. The table below is the current
working baseline used to prioritize the next patch releases.

| Area | Current Orca State | Codex/Claude Reference | Status |
|------|--------------------|------------------------|--------|
| Tool registry | Built-ins, MCP tools, and TOML external tools share `ToolSpec` metadata; runtime argument validation covers common object keywords plus `oneOf` / `anyOf` composition | Codex-style spec/capability registry | Implemented |
| Tool approval | Action kind is derived from tool capabilities, with TOML allow/deny rules | Capability/policy driven approvals | Implemented |
| File discovery | `glob` remains model-facing; interactive discovery now uses multi-root streaming `orca-file-search` with browse/fuzzy modes, exclude/Git-ignore controls, owned cancellation, million-path acceptance gates, and Codex-compatible app-server sessions | Claude `Glob`, Codex file search | Implemented |
| Mention system | Files, Skills, Plugins, MCP Resources, and Resource Templates share one typed candidate model in TUI and thread-bound app-server search; visible tokens carry hidden atomic targets that survive preceding edits, invalidate on overlap, and expand against the selected root/registry | Codex atomic structured input and unified mentions; Claude resource/file typeahead | Implemented |
| Shell execution | A thread-owned `TerminalService` exposes model-facing `exec_command` and `write_stdin` with retained session/task ids, optional PTY, raw control-character input, bounded incremental output, active permission-profile sandboxing, and immediate `task_stop` process-tree termination. A bounded-mailbox, single-owner supervisor actively reaps natural exits and registry stop requests without another poll, releases per-session resources, joins on shutdown, and injects exactly-once bounded completion notifications before the next model turn unless the terminal was already observed. Synchronous `bash` and JSONL server shell adapters remain compatible over the same low-level shell-session manager | Codex `exec_command` streaming/exit watcher; Grok Build exit watcher and completion notification | Implemented |
| Context management | BPE token counting, local compaction, persisted collapse/summary records, and a stable `ContextWindowId` per model-visible epoch | Multi-level local/remote compaction | Partial |
| Tool output control | Runtime task output uses a bounded, UTF-8-safe `TaskOutputStore` for shell and command/exec output, preserves cumulative streaming caps, and evicts terminal process output. v0.2.24 additionally caps ordinary child stdout/stderr at 1 MiB per stream before final tool-result truncation, preserves omission metadata, and bounds regular-file reads, exact edits, and committed TUI diff previews at admission | Codex bounded exec replay plus package 3 disk-backed task output and offset polling | Partial; v0.2.24 published, persistent offset polling remains open |
| Model metadata | `ModelSelection`, DeepSeek defaults, typed direct-vs-analysis image routing, and `deepseek-v4-flash-vision-exp` visual preprocessing for `auto`, Pro, and Flash | Codex `models-manager` with model capability metadata | Partial |
| MCP | stdio/SSE config surface, tool routing, read-only resource list/read/template tools, unified Mention discovery, same-registry Resource expansion, and v0.2.24 timeout/cancel/error/drop cleanup with bounded stdio response framing and process reaping | Codex MCP client/server ecosystem | Partial; resource/tool/Mention integration and lifecycle hardening seeded |
| Hooks | Lifecycle hooks with JSON stdout actions; structured outputs that declare `action` now validate supported actions and required string fields | Codex hooks runtime and schema validation | Implemented; schema docs/validation improved |
| Project instructions | User/project/rules files with includes | `AGENTS.md` style layered instructions | Implemented |
| Memory | Manual `/remember` plus optional project extraction | Codex memories extension | Partial |
| Execution budget | One `BudgetController` per operation owns independently optional `[budget]` dimensions (`max_turns`, `max_tool_calls`, `max_cost_usd_micros`, `max_wall_time_ms`); unlimited by default with no implicit 128-turn ceiling. Journal schema v2 durably records the immutable spec and cumulative `budget.usage`, so restart and provider-suspension settlement restore the same wall deadline and exactly-once provider accounting. A typed `OperationTerminal` (Completed/Stopped/Failed/Cancelled) is the one terminal fact every surface consumes; budget stops settle the current tool, create a checkpoint, and exit 4. Concurrent child agents atomically split the parent's actual remaining additive capacity under `BudgetLease`s, share one wall-clock deadline, and report complete consumed usage; the journal orders `tool.completed` → `checkpoint.created` → `operation.terminal` durably and restores unmatched tool starts as indeterminate. Goals own a cumulative token budget; exhausted Goal budgets disable automatic continuation | Codex/Grok explicit budgets, Claude Code checkpoints | Implemented |
| Persistent goals | Runtime-owned `GoalActor` and composite `GoalRun` with SQLite state, run/turn ledgers, verified terminal intents, recovery, cancellation, usage, semantic continuation admission, cumulative timing, and goal-scoped `get_goal`, `create_goal`, and narrow `update_goal`. There is no fixed turn ceiling; continuation count is observability data only | Codex goal extension plus explicit call context patterns in Claude Code and Grok Build | Implemented |
| Workflows | JavaScript workflow DSL, generated drafts, edit/save/run controls, reusable workflow commands, task state, notifications, runtime status events, evidence-bound reports, and worktree-isolated/recoverable agent runs | Codex/Claude workflow orchestration concepts | Implemented |
| Runtime lifecycle | Headless, server-mode, and TUI agent runs seed an agent task lifecycle through a runtime turn runner; `RuntimeThread` owns long-lived interactive state, while `GenerationContextController`, `InteractionController`, `OperationRecoveryController`, and `TaskWorkflowController` own focused state transitions and return typed decisions or event batches. Workflow, subagent, task, permission, workflow IPC, and normal-tool dispatch route through `RuntimeToolRouter`; external processes and search workers retain explicit cleanup ownership | Codex `Session -> Task -> Turn`, app-server request processors; package 3 pending permission maps | Implemented |
| TUI | Markdown-ish rendering, themes, Vim mode, bounded committed diff previews, slash commands, atomic unified mentions and `[Image #N]` attachments, background clipboard image reads, dragged/pasted image paths, composer previews, message-area image thumbnails, a zoomable/pannable true-color viewer, image-only and queued multimodal turns, workflow panel, per-turn timers plus cumulative active-Goal timing, truthful interrupted/indeterminate tool rows, mouse text selection with OSC 52 clipboard copy, double-click word copy, edge auto-scroll, and a jump-to-bottom control | Codex/Claude richer terminal UX | Partial |
| History | JSONL transcripts, resume/fork/search/archive/compress with a dedicated `SessionStore` boundary; resume normalization drops legacy reasoning-only assistant turns before provider replay | Codex thread store with queryable metadata | Partial |
| Release | GitHub release + npm alias distribution scripts, retrying post-publish GitHub/npm/npm-exec verification, and a reusable real API e2e release gate | Codex npm/native release model | Implemented |
| Skills | Markdown discovery, `list_skills`/`read_skill`, explicit `$skill` injection, and atomic unified Mention candidates across runtime workspace roots | Codex skills and plugin-provided skill bundles | Implemented for discovery/injection; plugin tool bundles remain open |

---

## Patch Release Plan

The next work should land as independent patch releases. Each release must be
verified before the next phase starts.

### Current Refactor Priorities

#### Evidence-Based Roadmap Reconsideration (2026-08-10)

This is the current planning baseline. It is based on a fresh inspection of the
source tree, git history, roadmap, and the 2026-08-03 architecture review. It
supersedes the ordering in the historical inventory below; completed work is
recorded as evidence, not repeated as future work.

#### Evidence: What Is Already Done

The first review tier is complete: `GoalActor::request` has a bounded
`recv_timeout` (`crates/orca-runtime/src/goal_actor.rs:1636`), supervisor store
access is moved behind `spawn_blocking`, streaming reduction appends with
`push_str` (`crates/orca-runtime/src/runtime_surface/reducer.rs:3678`), the
unused `orca-provider` dependency is gone from `orca-tui`, and usage projection
assigns values by event ordinal (`crates/orca-runtime/src/runtime_surface/commit.rs:3722`).

The v0.3.1 defect tier is also closed: the session lifecycle contract was
fixed (`959adeedf`), and the runtime-surface validator is executed by
`.github/workflows/runtime-contract.yml`. The larger architecture tier is
mostly closed: production `lib.rs` files no longer use source-layout
`include_str!` assertions (the remaining embeddings are workflow/host assets
and surface-manifest fixtures), `unstable_surface` is closed behind curated
exports, provider no longer depends on tools, and MCP registry ownership is in
runtime (`b81c4d413`).

ThreadActor has started the intended split. The four existing controllers under
`crates/orca-runtime/src/runtime_actor/` (`background`, `capability`, `commit`,
and `goal`) total 2,834 lines. This is meaningful progress, but it is not the
end state.

#### Evidence: What Is Still Open

1. **ThreadActor split completed on 2026-08-21.** The main
   `impl ThreadActor` is now below the approximately 8,000-line structural
   target. Goal, generation/provider, interaction, operation-recovery, and
   task/workflow implementations compile in separate modules. Dedicated
   controllers own generation context and compaction decisions, durable
   interaction and cold-recovery maps, operation retry state, task ownership,
   capabilities, commits, background work, and Goal state. `ThreadActor`
   retains mailbox dispatch, controller lifecycle, and cross-controller
   transaction coordination.
2. **P1.4 task supervision is implemented in the current slice.** Persistent
   `TaskRegistry` records now carry an owner lease, monotonically increasing
   fencing epoch, expiry, durable stop request, and publication revision.
   Lease acquisition, renewal, and worker terminal writes reload the indexed
   session under `ExclusiveFileLock`; stale owners are fenced after takeover.
   Async workers renew their lease while active, reapers only refresh local
   state, and persistent `list()` refreshes the complete session snapshot.
   The focused task lifecycle and recovered-worker tests cover these claims;
   the cross-process PTY and full workspace gates remain release evidence.
3. **TUI/runtime protocol drift is being sliced.** The `codex/tui-convergence`
   stream has established forty-four focused owners/boundaries so far (insert-escape, presentation,
   input-wake, workspace-config, scrollback, exit-policy, hosted-side,
   workflow-panel, transcript-search-orchestration, input-history,
   queued-submission, edit-highlight, surface-metrics, Goal projection,
   session-identity projection, workflow-task projection, hosted Goal
   orchestration, hosted session projection, hosted session lifecycle, hosted
   settings, hosted submission, hosted latest-active Goal recovery, hosted
   Goal action ownership, hosted session action ownership, hosted Side action
   ownership, hosted context action ownership, hosted workflow action
   ownership, hosted operation recovery ownership, hosted plan implementation
   ownership, hosted task action ownership, pending interaction input
   admission, interaction response acknowledgement, hosted controller
   ownership, renderer runtime-event ownership, renderer frame ownership,
   terminal session startup ownership, renderer input-wake ownership,
   renderer input-routing ownership, renderer interaction-acknowledgement
   ownership, renderer runtime-inbox ownership, renderer iteration-event
   routing ownership, foreground renderer-loop ownership, active terminal
   lifecycle ownership, and terminal bootstrap ownership); `app.rs` is currently
   7,692 lines,
   `renderer_loop.rs` 438 lines,
   `renderer_event_router.rs` 273 lines, `renderer_runtime_inbox.rs` 104 lines,
   `renderer_interaction_acks.rs` 182 lines,
   `renderer_input_router.rs` 461 lines, `renderer_input_wake.rs` 298 lines,
   `terminal_session.rs` 460 lines, `presentation.rs` 203 lines,
   `tui_run_lifecycle.rs` 69 lines, `renderer_frame.rs` 525 lines,
   `renderer_runtime.rs` 395 lines, and `hosted_controller.rs` 683 lines,
   `hosted_plan.rs` 247 lines, `hosted_operation.rs` 132 lines,
   `hosted_workflow.rs` 132 lines,
   `hosted_context.rs` 252 lines, `hosted_side.rs` 495 lines,
   `hosted_session_lifecycle.rs` 852 lines, `hosted_goal.rs` 404 lines,
   `background_tasks.rs` 239 lines, `idle_submit_actions.rs` 470 lines,
   `action_dispatcher.rs` 815 lines, `agent_runtime.rs` 262 lines,
   `runtime_event_actions.rs` 1,248 lines, and `types.rs` 8,936 lines. The
   activated terminal session now uniquely retains terminal, presentation,
   input runtime, and input-wake lifetime; it enforces initial pending title,
   first-frame draw, renderer body, and total cleanup in one consuming method.
   Reset failure cannot skip terminal retirement or input finish, and renderer,
   inbox, and agent shutdown run after every terminal outcome with explicit
   error precedence. The cold legacy registry boundary now imports immutable
   MainSession Completed/Stopped/Cancelled history and adopts safe Running
   MainSession rows under separate locked receipts and dedicated coordinator
   authorities. When the recovered surface has no prior operation lineage, a
   Running adoption receives a canonical non-replayable
   operation/generation/background fence, then existing cold recovery records
   restart abort and terminal reconciliation stops the task. Existing typed
   operation lineage makes an unlinked legacy row ambiguous and skips adoption.
   Queued, paused,
   stopping, approval, failed/retryable, workflow, subagent, shell, and monitor
   rows still require durable phase, interaction, ownership, graph, and rollback
   semantics before they can become visible or actionable. Live
   task/operation projection duplication has been removed from the TUI event
   boundary (`surface_projection.rs`, 3,280 lines).
4. **P2.4 context/cache identity is not a release slice.** DeepSeek usage
   already parses `prompt_cache_hit_tokens` (`crates/orca-provider/src/deepseek_http.rs:192`),
   but deterministic cache-critical prefixes (stable system prompt, tool
   schema, and conversation-prefix ordering), fork isolation, and explicit
   checkpoints are not yet specified as one independently verifiable change.
5. **The pending-store deletion gate passed on 2026-08-18.**
   The four interaction types pass on all four surfaces with exact fail-closed
   selectors and durable cold recovery. `RuntimePendingInteractionStore`, its
   crate export, and the no-op builder are deleted, and production/test source
   contains zero old symbols. Validator, runtime all-targets, and TUI checks
   pass; downstream Rust callers must migrate from the removed compatibility
   API.
6. **Repository cleanup (round 19, done).** The five linked cleanup
   candidates (`codex/auto-memory-governance`, `codex/headless-trajectory-truth`,
   `codex/mcp-sse-elicitation`, `codex/network-ask-on-block`,
   `feat/side-conversation`) no longer exist as refs, local or remote, and
   their replay commits (`97fa233c4`, `565b4be92`, `fd75c85bc`,
   `c69a8a263`, `8a7ae4584`) are all ancestors of `main` — the work is
   replayed, nothing to delete. Ten fully-merged slice worktrees and their
   branches (`compaction-remote-eval`, `integrate-reliability-slices`,
   `mcp-wire-elicitation`, `p1-4-task-supervision`, `p2-4-cache-identity`,
   `pending-store-retirement`, `threadactor-split`, `tui-runtime-convergence`,
   `tui-terminal-wait-cancellation`, `v0.3.14-review-fixes`) were
   provenance-checked (ahead=0, clean trees) and removed; the two active
   worktrees (`parallel-test-isolation`, `threadactor-capability-extraction`)
   remain.

#### Reordered Release Slices

Each row is an independent, behaviorally verifiable patch release. The order
follows the project decision priority: lifecycle and ownership, TUI reliability,
architecture boundaries, DeepSeek-native value, compatibility migration, slice
size, then short-term implementation cost.

| Order | Slice | Priority class | User value | Acceptance evidence |
|------:|-------|----------------|------------|---------------------|
| 1 | **P1.4 task supervision completion**: add `TaskRegistry` lease/fencing/stale-owner takeover and task-wide publication | Lifecycle / ownership | Background subagents can be stopped, reaped, reattached, and recovered after process failure without a detached owner | Cross-process PTY contract, crash-recovery test, stale-commit rejection, and focused/full task-lifecycle gates |
| 2 | **ThreadActor split completion (completed 2026-08-21)**: controller state has one owner and the main impl is below ~8k lines | Architecture boundary | Future runtime features are isolated from unrelated lifecycle state | Runtime controller traces, runtime-host integration suite, surface contract, and full workspace gates |
| 3 | **TUI/runtime protocol convergence**: extract renderer-owned orchestration and make runtime surface state the single projection source | Architecture boundary | Fewer TUI regressions and one authoritative rendering/lifecycle state | Real TUI PTY contracts plus the runtime-surface contract validator in CI |
| 4 | **P2.4 context/cache identity**: deterministic cache-critical prefixes, fork-state isolation, and explicit checkpoints | DeepSeek-native | Stable long sessions can realize prompt-cache savings instead of invalidating the prefix on incidental reorderings | Two real DeepSeek API requests with the same prefix and observed `prompt_cache_hit_tokens`, plus fork/checkpoint behavior tests |
| 5 | **Pending-store deletion gate (completed 2026-08-18)**: the compatibility shim and legacy pending-store API are removed | Compatibility migration | The durable broker is the only interaction fact source across all four surfaces | 4×4 interaction matrix, exact fail-closed selector and cold-recovery coverage, zero old symbols, validator, runtime all-targets, and TUI checks pass |
| 6 | **Compaction completion and remote-compaction evaluation**: finish `RuntimeCompactionPolicy`, then drive remote work from real waiting behavior | DeepSeek-native | Long conversations retain usable context without silently dropping state | Long-context real-API smoke, interruption/recovery checks, and focused/full compaction gates |
| 7 | **Repository cleanup**: remove the five superseded linked branches and integration residue | Hygiene | One clear source of truth for maintainers and release automation | `git merge-base`, patch/provenance checks, clean worktrees, and branch/worktree verification |

#### Why This Differs From the Previous Roadmap

- ThreadActor moves from the old third tier to the front because the evidence
  calls it Critical and every additional feature is still paying its cost.
- P2.4 cache identity moves into the first four slices because it is the rare
  DeepSeek-native capability with direct user cost impact and a concrete real
  API verifier.
- Pending-store retirement becomes an explicit acceptance slice rather than a
  vague later cleanup; a defined deletion gate is only useful when the roadmap
  schedules its verification.
- Worktree cleanup is now explicit and provenance-first: no branch is deleted
  merely because it is not an ancestor, and no unrelated user work is reset or
  merged into the release path.

Slice 1 is implemented from a freshly fetched `main` in
`.worktrees/p1-4-task-supervision` after its Spec Gate and implementation plan.
Its remaining release evidence is recorded in that plan before the next slice
starts.

The 2026-08-10 compaction evaluation closes slice 6 without adding a second
agent loop: the production remote-summary compactor passed a real DeepSeek
long-context smoke (34 messages to 9 and 4,992 to 1,216 wire tokens), while
focused runtime/provider gates cover waiting, cancellation, persistence, and
recovery. The reproducible evidence is in
`docs/reports/2026-08-10-compaction-remote-evaluation.md`.

These reliability slices shipped together in v0.3.14. That release included the
cross-process task lease/fencing boundary, ThreadActor state ownership
extraction, centralized TUI attachment routing, DeepSeek cache-prefix identity,
and the remote-compaction verifier. At that historical release point,
pending-store API removal was deliberately excluded; the gate later completed
on 2026-08-18.

#### Historical Refactor Inventory (Superseded)

The July 2026 Codex and package 3 reference pass ranks the remaining
architecture work as follows. Codex is the stronger reference for ownership:
core thread/session code runs turns against a frozen `TurnContext` plus
request-scoped `StepContext`, with dedicated owners for thread management,
compaction, exec policy, MCP runtime, skills, connectors, and protocol item
projection. Package 3 is useful for product surface and pending-request UX:
its task panel, bridge activity summaries, permission update destinations, and
MCP elicitation queue are good interaction references, but its broad
`ToolUseContext` and app-state-coupled orchestration should not be copied into
Orca.

At the time, the July 11 ownership and recovery pass superseded the older
priority labels below. The full evidence and dependency graph are recorded in
[`docs/reports/2026-07-11-codex-package3-runtime-refactor.md`](reports/2026-07-11-codex-package3-runtime-refactor.md).
The immediate sequence is:

1. close every tool invocation with exactly one terminal result, including
   cancelled and crash-indeterminate turns;
2. replace resettable shared cancellation with one-shot operation scopes and
   typed terminal outcomes plus stable operation identities;
3. finish the seeded runtime host and thread actors by migrating a surface,
   then move one canonical turn executor under that owner;
4. run the async provider directly from that host, then delete the temporary
   synchronous provider worker;
5. migrate server, headless, and TUI surfaces onto runtime command/event
   handles before declaring the executor canonical;
6. add a semantic execution journal with stable item ids and one thread event
   sequencer, then add the interaction broker, tool runtime, and fenced task
   supervisor before attempting true workflow/subagent/goal resume.

The detailed inventory that follows remains useful implementation history. New
releases now use the evidence-based sequence above rather than counting
additional call-surface bundles.

The deeper July 9 reference pass recorded the following refactor order at that
checkpoint:

1. **P0: Stop treating call-surface grouping as the main work once the current
   tool-turn context family is finished.** The normal, subagent batch, and
   readonly tool-turn contexts now follow the request/I/O/services/runtime
   grouping pattern. Further mechanical grouping should only happen when it
   unlocks a real owner boundary.
2. **P1: Promote compaction into a first-class runtime policy/task boundary.**
   Codex separates context-window accounting, token-budget reminders, pre/post
   compaction hooks, initial-context injection, retry metadata, and compaction
   telemetry. Orca currently has a lifecycle-owned `RuntimeCompactionStep`, but
   compaction remains a mostly synchronous summary-and-persist operation. The
   first slice is now seeded: `RuntimeCompactionPolicy` maps context pressure to
   explicit soft/hard triggers, and `RuntimeCompactionTask` records trigger plus
   before/after message counts before summary-state persistence. The task now
   finishes into `RuntimeCompactionOutcome`, which records local truncation vs
   remote-summary strategy plus structured reason/details data for later status
   and telemetry projection. Package 3 reinforces the next shape: prompt-too-long
   recovery should advance through named transitions such as collapse-drain retry
   and reactive compact retry, but Orca should keep those decisions inside the
   runtime compaction boundary instead of coupling them to the broad query loop.
   The retry decision slice is now seeded: provider and child-agent
   prompt-too-long recovery both carry `RuntimeCompactionRetryState` and ask
   `RuntimeCompactionPolicy` whether to compact and retry or surface the error,
   leaving provider code to execute a decision instead of reclassifying context
   errors. The event/TUI projection slice is also seeded: successful runtime
   compactions emit `context.compacted` from `RuntimeCompactionOutcome` details,
   and the TUI runtime event adapter maps that event into the existing compacted
   context notice/status path. The pre-status slice is now complete as well:
   automatic compaction emits typed `context.compaction.started` before hooks
   or summary work, and manual `/compact` enters the same TUI compacting state
   before its synchronous call. Completion now follows persistence and
   post-compact hooks, and TUI interrupt controls cancel the streaming DeepSeek
   summary path instead of becoming inert while `Compacting` is visible. Keep
   later lifecycle additions driven by real user-visible waits rather than
   adding speculative phases.
3. **P1: Move exec/permission evaluation toward a dedicated policy manager.**
   Codex keeps mutable exec policy in an `ExecPolicyManager` with parsed rules,
   command-origin lowering, prompt rejection reasons, and serialized updates.
   Orca already has permission profiles, turn/session grants, network proxy
   enforcement, and glob-based permission rules; the next architecture gain is
   to put rule matching, stricter rejection reasons, and future automatic
   network-block prompts behind a runtime policy owner instead of continuing to
   spread that logic across bash, command/exec, approval, and protocol paths.
   The first slice is now seeded with `RuntimeToolApprovalPolicy`, which owns
   preapproved tool-call consumption, permission-rule resolution, and strict
   auto-review downgrades before the tool execution gate handles events and
   interactive prompts. `RuntimeBashPermissionPolicy` now also owns bash-side
   escalation decisions: converting network proxy blocks into requestable
   network permission prompts while preserving denylist blocks as final
   diagnostics, converting sandbox write denials into filesystem grants, and
   converting pathless sandbox denials into unsandboxed shell retry prompts.
   `CommandExecPermissionPolicy` now starts the same cleanup for command/exec:
   streaming drains route filesystem retry decisions through the policy, and
   non-streaming command/exec network, filesystem, and pathless sandbox retry
   prompts now get their permission profile and reason text from that same
   policy before the server registers the pending request. The shared request
   construction is now promoted into `RuntimePermissionPolicy`, so bash and
   command/exec use one actor-scoped owner for network block, filesystem write,
   and pathless unsandboxed retry permission requests. The prompt-decision
   slice is also seeded: `RuntimePermissionPolicy` now decides whether
   `request_permissions` should auto-allow, prompt via a runtime handler, or
   reject with an explicit reason, preventing non-full-auto runtime paths from
   silently granting permissions when no handler exists. The command-origin
   metadata slice is now started as well: runtime permission construction can
   return a structured decision carrying origin plus request kind, and
   both bash and command/exec preserve that metadata when adapting retry
   prompts. The TUI bash path now consumes the same `RuntimePermissionPolicy`
   decision constructors for network, filesystem, and pathless unsandboxed
   retries instead of hand-building separate permission requests. The
   user-visible projection slice is now complete for TUI approvals: pending
   permission interactions preserve network/filesystem/unsandboxed request
   kinds, and approval dialogs show specific titles for those risks. Next mirror
   Codex more directly by using the same decision shape for future exec-policy
   rule evaluation instead of returning only prompt text.
4. **P2: Turn MCP elicitation and dynamic waits into pending interactions.**
   Package 3's MCP elicitation queue is the useful reference here: request id,
   server name, mode, abort signal, completion notification, and hook-driven
   auto-response are all explicit. Orca's typed runtime interaction boundary
   already covers approvals, `request_permissions`, and `request_user_input`;
   adding MCP elicitation or other dynamic waiters must use that typed surface
   and durable broker owner rather than creating a process-local queue. The
   compatibility record types still describe display fields, but
   `RuntimePendingInteractionStore` is no longer a production runtime owner.
   The first runtime boundary slice is now seeded: `RuntimePendingInteractionRecord`
   has an `McpElicitation` kind, carries server/request/mode/url/schema details,
   and builds stable server-scoped ids so duplicate MCP waits are rejected
   before a TUI or server surface can create an unrouteable second prompt. The
   typed runtime surface can project MCP elicitation records into a visible
   waiting-input state, preserve URL/form metadata, resolve only the matching
   runtime id, and clean up the pending record on answer or cancel. The stdio
   MCP transport and runtime tool path route real `elicitation/create` requests
   through that owner, write accept/decline responses back to the server, and
   continue the original tool call once the user answers. Server-mode now mirrors
   the same id
   discipline through `mcp_elicitation_request`, `mcp_elicitation/respond`, and
   `mcp_elicitation_resolved`, so remote and bridge clients can drive the same
   waiting interaction without a TUI-local queue. SSE tool calls now consume
   bounded response streams message-by-message, route `elicitation/create`
   through that same typed handler, POST the matching JSON-RPC accept/decline
   response, and only then return the terminal tool result. Cancellation still
   closes and joins the request worker, while JSON response bodies remain
   compatible. Transport accept/decline/malformed/timeout/cancellation tests
   and runtime MCP interaction contracts cover the parity boundary. A
   long-lived GET subscription remains out of scope until a user-visible
   server-push need justifies its additional connection owner.
5. **P2: Make skills/plugins a manifest-backed capability source only after
   policy and protocol owners are stable.** Codex's skills, connectors, and
   plugin managers are valuable, but adopting them before compaction, exec
   policy, MCP waits, and item projection settle would widen the surface too
   early. Keep Markdown skills stable; add manifests when plugin-provided
   tools can flow through the same policy, pending-interaction, and projection
   paths as built-ins, MCP, and TOML tools.
6. **P3: Borrow package-3 UX polish through runtime summaries, not app-state
   coupling.** Activity summaries, tool verbs, selected task details,
   permission destinations, and bridge-style remote status are worth copying
   only when the source of truth is Orca runtime task/thread/protocol state.

1. **P0: Runtime-owned background approval continuation execution.** Done on
   current main: the TUI now resumes approved background turns by converting the
   stored provider response into a typed runtime continuation and running a
   `ThreadTurnRequest` through the runtime bridge. The renderer-owned
   preapproved provider/tool loop has been removed, and the TUI only projects
   runtime events plus final task status.
2. **P1: Pending interactive request boundary.** Seeded: runtime now owns a
   focused `RuntimePendingInteractionRecord` shape for tool approvals,
   `request_permissions`, and `request_user_input`, and the TUI interaction
   adapter projects those runtime records into existing dialogs/prompts instead
   of hand-building separate payloads. Runtime also owns the shared pending
   interaction store, and the TUI session passes that store through tool
   approvals, `request_permissions`, `request_user_input`, and child-agent tool
   paths. TUI approval and user-input responses now carry the runtime
   interaction id back through the action channel, so handlers resolve only the
   matching pending request. The server protocol now also rejects
   `permission/respond` submissions that omit `requestId`, keeping cross-surface
   permission responses tied to a concrete pending request. Runtime and server
   pending-request maps now reject duplicate ids instead of silently replacing
   an existing waiter, and TUI interaction adapters now fail before prompting
   when a duplicate pending id would otherwise create an unrouteable second
   dialog. Server-mode `request_user_input` now follows the same Codex/package
   3-style pending map: the runtime emits `user_input_request`, clients answer
   with `user_input/respond` plus `requestId`, and the server resolves only the
   matching waiter with `user_input_resolved`. The v0.2.1 server processor
   slice keeps that response resolution owned by `server/processors/user_input.rs`
   and keeps `permission/respond` resolution owned by
   `server/processors/permission.rs`, so the generic server module no longer
   owns interactive response handling. Background main-session approval
   actions now also carry the pending tool approval request id through the TUI
   action channel; the runtime task registry validates that id, rejects
   duplicate responses, and returns the owning task id only after the request
   has been matched. Workflow terminal notifications now carry a stable
   notification id derived from runtime workflow ids through the TUI queues, so
   batch-boundary reconciliation no longer identifies pending continuations by
   prompt text; both AppState and batch-boundary queues now reject duplicate
   notification ids before creating duplicate model continuations or user-visible
   notices. The cross-thread TUI notification queue is now a focused
   `PendingWorkflowNotificationQueue` boundary instead of an exposed
   `Arc<Mutex<VecDeque<_>>>`, so queue insert/drain/pop behavior stays behind
   named methods. Queued workflow continuations also keep their notification id
   when they cross the TUI action channel or return from a turn-result
   continuation; human prompts remain plain `Submit` actions, while workflow
   follow-ups use a typed notification action/result. The workflow-notification
   action channel now carries `PendingWorkflowNotification` directly instead of
   splitting the id and prompt into separate action fields, and workflow
   notifications enter `SubmittedTurn` through the same typed notification
   boundary. The TUI agent loop now applies prompt preprocessing through a named
   submitted-turn source boundary, so `@file` mention expansion remains
   user-input behavior and workflow notifications are not dropped because
   generated notification text happens to look like a local file mention. That
   source boundary also carries the TUI task description for workflow follow-up
   turns, keeping the workflows panel focused on a stable notification label
   instead of raw XML/diagnostic payload text. The same submitted-turn boundary
   now also gives first-turn workflow notification sessions a stable title seed,
   so recorded history/search does not name the thread after raw notification
   XML. Workflow follow-up turns remain model-visible user-role context, but are
   no longer treated as the user's last backtrack target. That submitted-turn
   value now enters the TUI goal-turn loop as one boundary object, with
   `SubmittedTurnPresentation` owning the task label and backtrack policy that
   had been passed as parallel fields. `SubmittedTurnKind` now owns the prompt
   and source-specific workflow notification state, leaving presentation metadata
   as a display/backtrack policy layer instead of a third parallel source of
   turn identity; the goal loop now reads that policy through submitted-turn
   accessors instead of reaching into presentation fields. That boundary now
   lives in a focused `submitted_turn` module instead of the app event loop
   file, and its presentation metadata type is private behind the submitted-turn
   accessors. Turn results now expose a typed `TuiAgentTurnContinuation`
   boundary instead of a workflow-notification-specific result field, so
   workflow follow-ups are one continuation variant and future continuation
   kinds do not need more parallel ad hoc result slots. Approved background
   turns also cross from the TUI approval handler into the continuation runner
   as a typed `TuiBackgroundTurnContinuationRequest`, so the runner no longer
   exposes a naked task id as its continuation boundary and denied approvals do
   not manufacture continuation requests. The TUI background approval response
   submission path now also lives in a focused module, keeping request-id
   matching, denied-task finishing, task-list refreshes, and typed continuation
   request creation behind one named boundary instead of embedding that state
   transition in the app event loop. Workflow terminal notification queueing,
   cross-thread pending notification draining, pending notification submission,
   and by-id removal now also live in a focused TUI workflow notification
   module, so the app event loop coordinates notification turn boundaries
   without owning the pending-continuation queue mechanics. Foreground and
   background approval option resolution now also lives in a focused TUI
   approval action module, keeping session allowlist updates and request-id
   action dispatch out of the app event loop while preserving the runtime
   pending-interaction ids. Next, move the same id discipline into
   remaining turn/item continuation ownership so continuations stop depending on
   separate ad hoc task fields plus TUI-local queues.
3. **P2: Frozen per-turn context boundary.** Continue shrinking wide call
   surfaces into `RuntimeTurnContext`, `RuntimeTurnDeps`,
   `RuntimeTurnState`, and request snapshots. Runtime turn continuations now
   live with the other immutable turn inputs inside `RuntimeTurnContext`, and
   runtime steering handles now enter the turn through the same config
   boundary, so `AgentLoopContext` no longer carries either as a separate ad
   hoc field. Turn-scoped permission and user-input handlers now live with the
   other injected services in `RuntimeTurnDeps`, keeping TUI interaction
   routing on the same dependency boundary as server/headless turns. Turn-loop
   workflow refs now pass through `RuntimeTurnWorkflowContext` instead of
   parallel background-workflow and workflow-IPC fields, and event output refs
   now pass through `RuntimeTurnOutputContext` instead of parallel
   `EventFactory`/`EventSink` fields. Turn-loop provider/model refs now pass
   through `RuntimeTurnProviderContext` instead of parallel provider,
   provider-config, model, and budget fields, and immutable request inputs now
   enter the turn loop as the lifecycle-owned `RuntimeTurnContext` wrapped by
   `RuntimeTurnRequestContext` instead of re-expanding cwd, prompt,
   continuation, steering, and subagent fields. `RuntimeAgentTurnLoopInput`
   now enters the loop through those same provider/request contexts instead of
   rebuilding parallel fields at the loop boundary, and turn-loop stages now
   pass injected services through `RuntimeTurnDeps` instead of repeating hooks,
   instruction, memory, MCP, and interaction fields. Turn-loop policy/config
   refs now pass through `RuntimeTurnPolicyContext` instead of repeating run
   config, directive-resolved tool policy, and approval policy fields.
   `RuntimeStepSnapshot` now also keeps those immutable turn inputs behind the
   same lifecycle-owned `RuntimeTurnContext`, so provider-response and tool-turn
   paths no longer read cwd, depth, or delta flags from a second flattened
   snapshot shape. `RuntimeProviderCycleInput` now follows the same boundary,
   handing provider-cycle steps cwd, delta emission, and steering through
   `RuntimeTurnContext` instead of duplicating those turn-entry refs.
   `RuntimeProviderTurnInput` now also consumes that turn context directly, so
   provider-call hook, streaming-delta, and steering setup no longer receive a
   second flattened copy of the same turn-entry data. `RuntimeTurnOpeningInput`
   now also consumes the lifecycle-owned `RuntimeTurnContext`, keeping
   compaction, turn-start, model-route, and steering setup on the same immutable
   turn-entry boundary. `RuntimeModelRouteInput` now also routes through that
   context, so model routing no longer duplicates the turn's subagent type or
   delta-emission flag. `RuntimeTurnStartInput` now follows the same boundary,
   so turn-start prompt selection and start-event emission read prompt and
   delta policy from `RuntimeTurnContext`. `RuntimeCompactionStep` now also
   carries `RuntimeTurnContext`, so budget-warning hooks, compaction events, and
   compaction history persistence no longer receive a second flattened cwd/delta
   copy. `RuntimeSteerInput` now also carries `RuntimeTurnContext`, so steer
   draining no longer receives a separate steer-handle copy. `RuntimeToolTurnsContext`
   now carries tool-turn I/O refs through `RuntimeToolTurnsIo`, keeping tool
   dispatch state distinct from event, transcript, cost, and workflow mutation
   refs. `RuntimeToolTurnsContext` now also carries child-agent dispatch
   executors through `RuntimeToolTurnsExecutors`, keeping normal, workflow, and
   batch child execution handles distinct from dispatch state and I/O.
   `RuntimeNormalToolTurnContext` now also carries normal tool execution I/O refs
   through `RuntimeNormalToolTurnIo`, keeping its execution snapshot distinct
   from event, transcript, cost, and workflow mutation refs.
   `RuntimeNormalToolTurnContext` now carries normal and workflow child-agent
   executors through `RuntimeNormalToolTurnExecutors`, so executor handles no
   longer sit beside snapshot and policy refs as flat fields.
   `RuntimeNormalToolTurnContext` now carries instructions, memory, MCP
   registry, and hooks through `RuntimeNormalToolTurnServices`, matching the
   downstream tool-execution services boundary.
   `RuntimeNormalToolTurnContext` now carries cancel, task registry, and
   workflow IPC refs through `RuntimeNormalToolTurnRuntime`, matching the
   downstream tool-execution runtime boundary.
   `RuntimeNormalToolTurnContext` now carries permission and user-input
   handlers through `RuntimeNormalToolTurnInteractions`, keeping lifecycle
   interaction hooks distinct from execution snapshot, services, runtime refs,
   and executors.
   `RuntimeNormalToolTurnContext` now carries config, cwd, tool request,
   subagent depth, delta policy, and approval policy through
   `RuntimeNormalToolTurnRequest`, leaving the context to compose named request,
   I/O, service, runtime, interaction, extension, and executor surfaces.
   Subagent batch tool-turn execution now enters
   `run_subagent_batch_tool_turn` through `RuntimeSubagentBatchToolTurnContext`
   and its request, I/O, service, and runtime groups, so the TUI-shared
   subagent batch path no longer exposes a long runner argument list.
   Readonly tool-turn execution now carries its request, I/O, and service refs
   through `RuntimeReadonlyToolTurnRequest`, `RuntimeReadonlyToolTurnIo`, and
   `RuntimeReadonlyToolTurnServices`, matching the normal and subagent batch
   context pattern on the TUI-shared dispatch path.
   Iteration stages now keep lifecycle-owned `RuntimeTurnLoopIterationState`
   grouped instead of unpacking runtime system messages, model overrides,
   cost/cancel/task refs, and extension refs into the iteration input. Keep
   borrowing package 3's explicit loop-local `State` idea, but avoid a single
   giant context object.
4. **P3: Protocolized task/thread/interactive status.** Push background task,
   approval-needed, needs-input, foregrounded/backgrounded, and completed
   status through runtime protocol events so TUI, server, and future app
   clients stop inferring state from surface-specific structs. The runtime
   event schema now has a single-task `task.status.updated` event, and TUI
   main-session task start/background/finish and background provider-completion
   updates, plus approved background-turn continuation refreshes, route through
   it instead of borrowing the workflow task-list event for each one-task status
   change. TUI subagent task creation, progress, and terminal status updates
   now use the same single-task event path. Workflow launch/startup terminal
   updates and background terminal updates now also use that single-task event
   path when a concrete workflow task id is known, while workflow progress
   polling keeps the aggregate workflow task-list event for full-list progress
   refreshes.
   Server protocol event mapping also preserves that single-task status event
   as `task_status_updated` for non-TUI clients. The TUI projection now keeps
   that path as a single-task update and merges it into the panel by task id, so
   one status event cannot drop unrelated visible tasks. Task activity is now
   derived through one `TaskActivitySummary`: a completed foreground turn can
   return the composer to its idle/interactive state while the activity line
   continues to show active background work and approval-required task counts.
   Runtime completion publishes the complete task registry before
   `session.completed`, so that derivation cannot briefly converge on an empty
   or stale task set.
5. **P4: Persistence policy for pending background continuations.** Seeded:
   approval-required background main-session tasks now persist a compact
   provider-response continuation record through `TaskRegistry`, so a restarted
   TUI session can recover the pending tool approval, accept the approval
   response, and resume through the runtime-owned continuation path instead of
   losing the provider response. TUI session initialization now also refreshes
   recovered approval-required background tasks and emits a user-visible notice
   naming the pending tool. Invalid or future-incompatible continuation records
   now fail closed at task-registry load time: the affected background task is
   marked failed, pending approval state is cleared, and the sanitized record is
   written back instead of blocking the whole session restore.
6. **P5: Package-3-style task UX polish.** Borrow the visible task panel ideas:
   sorted task list, detail view, foreground/stop actions, and notifications.
   Keep implementation behind Orca runtime task/protocol types rather than
   importing package 3's UI-state coupling. Seeded: the TUI task panel can now
   request a stop for the selected non-terminal task through the runtime
   `TaskRegistry`, refreshing the panel after the status changes. Stop,
   foreground, and recovered-background-approval task actions now live in a
   focused TUI background task module, keeping package 3-style task controls
   behind Orca runtime task summaries instead of app-loop state mutation. The
   workflows panel key handler now also lives in a focused TUI panel action
   module, so task selection, approval opening, stop dispatch, and foreground
   dispatch are grouped with the panel UX instead of the app event loop.
   Running-state shortcuts now also execute through a focused TUI running
   action module, keeping background-current-turn, interrupt, and live-scroll
   behavior grouped with the running UX instead of the app event loop.
   Composer textarea construction, prefilled text restoration, text extraction,
   setup input masking, and paste insertion now live in a focused TUI composer
   module, giving slash/mention/menu input flows one shared input boundary.
   Mention candidate refresh and mention menu key handling now live in a
   focused TUI mention action module, keeping @file completion state changes out
   of the app event loop.
   Slash command execution now lives in a focused TUI slash command action
   module, so direct command submission and menu completion share one
   configuration/state mutation boundary.
   Slash menu candidate refresh, menu key handling, selected command dispatch,
   and model/reasoning submenu flow now live in a focused TUI slash menu action
   module, leaving the app loop to route input events rather than own menu
   mechanics.
   Composer input editing now lives in a focused TUI composer input action
   module, covering slash/mention refresh after edits, newline handling,
   history recall, Tab file mention completion, and plain key input.
   Idle submit handling now also lives in a focused TUI idle submit action
   module, covering slash-command short-circuit submission, pending
   user-input answers, normal prompt submission, prompt-history recording, and
   composer reset after accepted submissions.
   Idle navigation/control shortcuts now live in a focused TUI idle navigation
   action module, covering scroll/page movement, backtrack dispatch, and
   expand-latest-tool-output fallback into normal composer editing.
   Global TUI shortcuts now live in a focused global action module, keeping
   Ctrl-C interrupt/exit flow, shortcut overlay toggling, transcript top/bottom
   scrolling, and clear-screen terminal cleanup out of the app event loop.
   Approval dialog key handling now lives in a focused TUI approval dialog
   action module, covering direct numeric/legacy option keys, selection
   movement, confirmation, and approve/deny shortcut resolution.
   Approval mode cycling now lives in a focused TUI approval mode action
   module, keeping Shift+Tab mode transitions, shared config updates, status
   cell updates, and user-visible notices out of the app event loop.
   Session picker key handling now lives in a focused TUI session picker action
   module, covering picker navigation/search, selected-session resume config
   updates, transcript projection, preloaded transcript storage, and terminal
   cleanup after resume.
   First-run setup key handling now lives in a focused TUI setup action module,
   covering setup-step transitions, API key persistence/config propagation,
   masked setup input editing, completion exit flow, and optional initial
   prompt submission after setup.
   Bracketed paste and mouse transcript scrolling now live in a focused TUI
   input event action module, keeping paste insertion/menu refresh and
   mouse-wheel transcript scrolling grace checks out of the app event loop.
   Key event preflight now lives in a focused TUI key event action module,
   keeping press/repeat filtering, global shortcut routing, shortcut overlay
   dismissal, approval-mode cycling, and workflow-panel escape handling out of
   the app event loop.
   Runtime event draining now lives in a focused TUI runtime event action
   module, keeping allowlisted auto-approval, backtracked prompt restoration,
   workflow-notification batch routing, state updates, and auto-scroll follow
   handling out of the app event loop.
   Idle key routing now lives in a focused TUI idle key action module, keeping
   slash menu, mention menu, workflow panel, idle shortcut, history recall,
   navigation, submit, and composer fallback dispatch out of the app event
   loop.
   Status-specific key routing now lives in a focused TUI status key action
   module, keeping setup, session picker, approval dialog, idle/user-input, and
   running shortcut dispatch out of the app event loop.
   Terminal lifecycle cleanup now lives behind a focused TUI cleanup guard,
   ensuring keyboard enhancement, bracketed paste, mouse capture, cursor
   visibility, and raw mode are restored even when the app exits through an
   early error path.
   Runtime task summaries now also expose terminal `result`/`error` fields so
   the selected task row can show completion output or failure details in the
   panel. The
   panel now renders contextual action hints for selection, approval, stop, and
   closing so TUI users can discover task controls in-place. Selected task
   result/error details now render as bounded multi-line summaries, keeping
   longer terminal output readable without letting one task consume the panel.
   Task refreshes now sort the panel by attention priority (approval-required,
   active, then terminal with recent activity first) while preserving the
   selected task by id across refreshes. Backgrounded running main-session
   tasks can now be returned to the foreground from the panel with `f`, clearing
   foreground-output suppression and refreshing the task list through
   `TaskRegistry`; the detached background provider worker now replays buffered
   visible reasoning/message/tool-progress deltas generated while hidden,
   forwards future deltas after foregrounding, and emits the normal foreground
   session-completed event when that turn finishes. When a main-session turn is
   first backgrounded, the TUI now opens the task panel and selects that
   backgrounded session once, making the foreground/stop controls discoverable
   without stealing selection on later refreshes. When that selected
   backgrounded session is returned to the foreground, the TUI closes the task
   panel so replayed and future assistant output is visible immediately.
   Backgrounded main-session approvals now also reveal and select their task
   once, so an approval wait is visible without clobbering later manual
   selection.

### P0: Session Runtime Unification

**Release target:** v0.1.31

**Current status:** done in v0.1.31.

**Goal:** move long-lived interactive session state from the TUI bridge into
`orca-runtime`, creating the runtime boundary needed for a Codex-style protocol
layer.

**Deliverables:**

- Add `orca_runtime::session::InteractiveSession`.
- Centralize conversation, history writer, session id, project instructions,
  memory, hooks, MCP registry, cost tracking, and workflow task registry in
  runtime.
- Keep `TuiConversationSession` as a compatibility wrapper that delegates to the
  runtime session.
- Preserve current TUI event names, JSONL behavior, workflows, goals, backtrack,
  compaction, and request-user-input continuation.
- Document the boundary in
  `docs/superpowers/specs/2026-06-25-session-runtime-unification-design.md`.

**Verification:**

- `cargo fmt -- --check`
- `cargo test --workspace --all-targets`
- `npm --prefix site run build`
- `npm --prefix site run check:seo`
- `node scripts/release/test-stage-npm.mjs`
- `git diff --check`
- Post-publish `scripts/release/verify-published.mjs` for GitHub Release, npm,
  and `npm exec` smoke verification.

### P1: Protocol And Event Boundary

**Release target:** v0.1.32

**Current status:** server-mode submissions and server-facing events now flow
through `orca_runtime::protocol` with typed `Submission`, `ClientOp`, and
`ServerEvent` values while preserving the legacy flat JSON wire format. The
server accepts the original `{"op":"submit"}` wire shape plus Codex-style
`thread/start` and `turn/start` method requests for the first app-server-shaped
thread/turn lifecycle entry points. Server-mode `turn/start` now parses
`params.threadId` and rejects unknown in-process thread ids, while persistent
ThreadStore-backed materialization remains a follow-up. The current development
baseline also exposes multi-root `fuzzyFileSearch/sessionStart|Update|Stop`,
thread-bound `mention/search/start|update|stop`, streamed candidate targets,
and structured atomic Mention input on `turn/start`.

**Goal:** introduce a runtime protocol boundary so TUI/headless clients can send
commands and consume versioned events without owning turn execution details.

**Scope:**

1. Define an `orca-runtime` protocol module inspired by Codex protocol types. Done in v0.1.32 for server mode.
   - User input, approval responses, cancel/backtrack, goal operations, and
     workflow controls should be commands.
   - Session lifecycle, assistant deltas, reasoning, tool calls, workflow/task
     updates, approvals, errors, and completion should be events.
2. Add a runtime event adapter. Server-mode adapter done in v0.1.32; TUI
   assistant deltas, usage, errors, session completion, tool-call
   requested/completed, plan-updated, and subagent started/completed events now
   adapt from runtime `EventFactory` payloads instead of hand-built TUI structs.
   - Preserve existing display behavior while sourcing events from runtime where practical.
   - Runtime approval events now carry concrete tool name, target, and preview
     metadata needed by TUI prompts, and interactive approval prompts flow
     through the adapter without losing UI fidelity.
   - Workflow terminal notifications now flow through runtime
     `workflow.result.available` / `workflow.failed` events with workflow name,
     tool-use id, status, and summary metadata. The server/runtime event schema
     retains `workflow.tasks.updated`; TUI task-list/progress facts now derive
     from committed surface task/workflow patches and arrive in the final
     `SurfaceProjectionSynced` snapshot. Declared workflow lifecycle events for
     resume, phase start/completion, agent start/cache/completion/failure,
     pause, and stop now have `EventFactory`, server protocol, and TUI notice
     coverage.
   - Keep JSONL output names stable for this release unless explicitly versioned.
3. Move more turn-loop orchestration behind runtime-owned APIs. Seeded after v0.1.42.
   - The TUI may still render and request approvals.
   - Runtime should own command handling and event emission. The current
     `RuntimeTaskActor` seed owns turn budget checks, turn advancement,
     `turn.started` event construction, model routing, pre/post model hook
     orchestration, provider streaming calls, and usage/budget accounting for
     controller turns. It also owns shell tool lifecycle event shaping so
     controller call sites no longer construct shell task payloads directly,
     owns pre/post tool hook context and warning/error formatting, and resolves
     non-interactive tool approval decisions. Normal built-in/external/MCP tool
     execution fallback also flows through the actor now. Tool approval,
     pre/post tool hooks, and normal fallback execution share one
     `RuntimeToolActorContext` instead of constructing ad hoc controller-owned
     lifecycles. Runtime-special tool dispatch classification for workflow,
     subagent, workflow IPC, and normal tool paths also now lives on that
     actor. `AgentLoopContext` now delegates immutable turn entry values to
     `RuntimeTurnContext` and read-only agent-loop services to
     `RuntimeTurnDeps`, the first package-3 `QueryConfig` / `QueryDeps`-style
     split, with mutable per-turn runtime handles grouped behind
     `RuntimeTurnState` and execution/lifecycle refs grouped behind
     `RuntimeTurnExecution`. Workflow IPC execution now flows through a runtime IPC trait on
     the context, SubagentStatus execution now flows through a runtime status
     lookup trait, and WorkflowDraft preview creation now lives on the runtime
     context. Workflow draft actions and launch now live in
     `workflow_execution`, subagent sync/async launch and worker entrypoints now
     live in `subagent_execution`, and the controller no longer owns those
     execution bodies.
4. Seed a first runtime-owned task/turn lifecycle. Done after v0.1.42 for
   headless agent runs, server-mode submissions, and TUI bridge turns:
   `turn.started` JSONL events, legacy server `turn_started` events, and
   `TuiEvent::TurnStarted` now carry task metadata. Workflow lifecycle events
   and synchronous `subagent.started`/`subagent.completed` events also carry
   task metadata.
5. Add a runtime-owned `RuntimeTurnRunner` seed. Done after v0.1.42 for
   headless controller turns and TUI bridge turns: turn advancement and
   `turn.started` task payload construction now live in `orca-runtime`.
   Async subagent workers now persist task lifecycle metadata through
   `subagent_status` results; workflow child agent evidence and shell tool
   call events now carry task lifecycle metadata too. A `RuntimeTaskActor`
   seed now owns controller turn starts, max-turn exhaustion, model routing,
   pre/post model hook orchestration, provider streaming calls, and
   usage/budget accounting, plus shell tool requested/completed event shaping
   and pre/post tool hook orchestration. Non-interactive tool approval
   resolution and normal tool execution fallback now flow through the actor
   too; these controller tool phases now reuse a single `RuntimeToolActorContext`.
   Runtime-special workflow/subagent/workflow-IPC dispatch is classified by the
   runtime context, workflow IPC execution now lives behind a
   `RuntimeWorkflowIpc` trait on that context, and SubagentStatus execution now
   lives behind a runtime status lookup trait. WorkflowDraft preview creation
   also now lives on the runtime context. Workflow draft actions, workflow
   launch, subagent launch, and async subagent worker entrypoints have been
   extracted from the controller into focused execution modules. Interactive
   approval resolution now flows through runtime approval handlers, so
   headless `tool_execution` and TUI tool execution share the runtime approval
   boundary while each surface supplies its own user-action adapter. TUI
   `request_user_input` continuations now use a runtime user-input handler
   boundary as well, leaving the TUI responsible for presenting and collecting
   user actions while runtime owns argument parsing and tool-result shaping.
   A `RuntimeShellSessionManager` seed can now spawn shell tasks with stdin,
   collect stdout/stderr, kill the process group, and keep `TaskRegistry`
   shell records in sync. Model-facing bash execution and server protocol
   operations now route through that shell-session boundary. Server
   `shell/read` now returns a running snapshot with available stdout/stderr
   without waiting for process completion. `shell/start` now accepts explicit
   `terminalMode: "pipe" | "pty"` configuration, preserves legacy `pty: true`,
   and can seed Unix PTY window size with initial `cols` / `rows`; `shell/resize`
   can still update Unix PTY window size after start. Shell reads now also emit
   Codex-style `shell_output_delta` notifications
   before the legacy `shell_updated` / `shell_completed` responses, and
   terminal shell reads/kills emit `shell_exited` with normalized process exit
   codes, including Unix signal exits as `128 + signal`. Active MCP tool waits now
   observe server-turn cancellation and let interrupted turns complete without
   waiting for the MCP transport's default request timeout. MCP stdio/SSE
   transports now accept configurable startup/tool request timeouts, and stdio
   requests use a reader-thread boundary so slow `tools/call` responses time out
   without blocking on `read_line`. SSE timeout behavior now has transport
   contract coverage, and legacy app-server `tool_completed` events preserve
   MCP timeout errors plus runtime `exit_code` and `kind` metadata. MCP clients
   now refresh the underlying transport after
   timeout/connection failures so later calls can recover without silently
   replaying the failed tool call. Shell sessions now expose requested and
   effective terminal modes so non-PTY platforms can fall back to pipe mode
   without making the session untestable. Server `shell/list` now returns
   active shell snapshots with task ids, commands, status, terminal modes, and
   descriptions, while `shell/update` can rename an active shell description
   and have the updated metadata reflected in later list snapshots. Codex-style
   `command/exec` and `command/exec/terminate` are now compatibility entries
   on top of the runtime shell-session manager for buffered commands and
   killable process ids, including request-scoped `cwd`, env override / unset
   handling, Codex `tty` field parsing, Codex-style validation for mutually
   exclusive timeout/output-cap and sandbox/profile options plus
   streaming-without-process-id requests, buffered and streaming output-cap
   truncation with streamed `capReached` metadata, streamed stdout/stderr
   deltas with `command/exec/write` stdin support,
   client-driven `command/exec/read` drains for active process handles, and
   `command/exec` TTY initial size/resize support, and read-time
   `outputBytesCap` tightening for active streaming process drains.

**Refreshed reference-driven priority order (Codex + package 3):**

1. **Shell/PTY task sessions:** Codex exposes long-running exec flows and
   package 3 models shell work as `LocalShellTask`; Orca now has a runtime
   shell-session seed with task ids, stdin, kill, output collection, nonblocking
   incremental reads, explicit pipe/PTY terminal modes, initial PTY sizing,
   PTY resize, bash-tool routing, and server
   `shell/start|write|close|resize|read|kill|list` operations. Shell start now
   reports requested/effective terminal modes, PTY requests fall back to pipe
   mode on platforms without PTY support, `shell/list` returns active shell
   snapshots for reconnecting clients, and `shell/update` can refresh the
   user-facing shell description metadata. Codex-style `command/exec` buffered
   execution and `command/exec/terminate` process-id cancellation now reuse the
   same manager rather than a second process runner, and `command/exec` now
   honors Codex-style `cwd`, env override/unset, `tty`, invalid option
   validation, output caps, streamed stdout/stderr delta, `command/exec/write`,
   explicit `command/exec/read` drains, and TTY initial size/resize request
   fields. Server shell reads now emit
   `shell_output_delta` and `shell_exited` notifications alongside legacy
   shell responses, giving clients a Codex-shaped process stream seed. The
   model-visible package-3-inspired `task_list` / `task_stop` tools now expose
   session tasks through `TaskRegistry` with `subject`, `status`, `owner`, and
   `blockedBy` fields plus Orca task type/command metadata, and `task_stop`
   accepts the deprecated `shell_id` alias while validating missing, unknown,
   and terminal tasks. MCP and TOML external tools now have first-class
   app-server item streams, and historical projections preserve failed
   MCP/dynamic tool status, error message, exit code, and truncation metadata.
2. **App-server turn controls:** Codex SDK tests cover steer/interrupt/resume
   at the turn handle level. Orca now accepts server `turn/interrupt`,
   `turn/resume`, and `turn/steer` commands, returns a stable
   `turn_controlled` event for idle/no-active-turn requests, runs
   thread-bound server turns in the background, and lets `turn/interrupt`
   cancel an active server turn so it completes as cancelled. Completed turn
   controls now return structured errors, and active controls can reject a
   mismatched `threadId` precondition. `turn/resume` can now reset a cancelled
   active token before the cancellation checkpoint observes it, and active
   `turn/steer` now emits an observable user `item_started` event and injects
   the steer input into the active turn's model context before the provider
   call. Active server turn handles now use the same user-visible persisted
   `turnId` that `thread/turns/list` exposes, with system messages excluded
   from user turn numbering. Pre/post model and tool hook subprocess waits now
   observe active turn cancellation, while provider streaming already receives
   the same cancel token. Bash shell-session tool waits and TOML external tool
   process waits now also observe the active turn cancel token, kill the child
   process group, and let interrupted turns complete as cancelled without
   waiting for the shell timeout. TUI-local streaming bash now shares the same
   cancel-aware process wait, preserving partial output while interrupting
   promptly. MCP tool execution now also observes the active turn cancel token
   and returns a cancelled tool result promptly instead of blocking the turn on
   the MCP request timeout. MCP server config now supports
   `startup_timeout_ms` and `tool_timeout_ms`; stdio/SSE `tools/call` timeouts
   are enforced at the transport boundary, and server-mode turns surface
   timeout details in legacy `tool_completed.error`. After timeout/connection
   failures, MCP clients rebuild the transport for future calls without
   automatically replaying the failed call. `shell/capabilities` now exposes
   the platform/runtime capability surface that clients need before requesting
   PTY sessions or resize operations, and `shell/read` can now cap incremental
   stdout/stderr with `outputBytesCap` plus `capReached` metadata for clients
   that need bounded reads, and `command/exec/read` now gives server clients a
   request/ack boundary for draining active streaming process output, with
   read-time `outputBytesCap` support for bounded polling. Next, use
   those boundaries for deeper
   cross-platform PTY support.
3. **ThreadStore-backed app-server materialization:** Codex treats threads as
   resumable/forkable SDK objects. Orca has `SessionStore` and an in-process
   server path; `thread/start` is now immediately visible through the
   persistent `SessionStore`, and server `thread/resume` / `thread/fork` can
   materialize live thread handles from persisted transcripts. Server
   `thread/resume` now reopens the same persisted thread id and appends future
   turn items to the original transcript, while `thread/fork` still creates a
   child transcript.
4. **Permission profile persistence:** Codex preserves approval mode across
   thread resume/fork/turn overrides, while package 3 keeps permission modes and
   rule sources as pure types. Orca now snapshots approval mode and permission
   rules into thread metadata and exposes `approvalMode` /
   `permissionRuleCount` through thread summaries. Server `thread/resume` and
   `thread/fork` now inherit the stored approval mode and permission-rule
   snapshot when materializing a live thread, and explicit app-server
   `approvalMode` / `permissionRules` resume/fork parameters override that
   snapshot when supplied. Codex-style `approvalPolicy` is now accepted as an
   app-server alias for thread resume/fork and turn/start requests, mapping
   `never` to Orca `full-auto` and `on-request` / `untrusted` to `suggest`;
   thread-bound `turn/start` permission overrides are applied to the active
   turn, persisted back to thread metadata, and visible in later thread
   summaries. Package-3-style `permissionUpdates` now decode on app-server
   `turn/start` and apply as ordered incremental updates after any whole-profile
   override: `setMode`, `addRules`, `removeRules`, and `replaceRules` map to
   Orca approval mode and permission-rule metadata, including package 3
   `Bash` / `Write` tool-name normalization and `ask` -> `prompt` behavior
   mapping. `addDirectories` / `removeDirectories` now persist package-3-style
   additional working directories on thread metadata, expose
   `additionalWorkingDirectories` / `additionalWorkingDirectoryCount` through
   `thread/read` and `thread/list`, and feed those roots into the bash
   seatbelt profile so the metadata changes real shell sandbox behavior.
   Codex-style built-in `permissionProfile` names (`read-only`, `workspace`,
   and `danger-full-access`, with or without the `:` prefix) now also drive
   `command/exec` sandbox selection. Thread-scoped `activePermissionProfile`
   is inherited by thread-bound `command/exec` when no request-level sandbox
   override is supplied, while explicit request `sandboxPolicy` still wins.
   Configured `[permission_profiles.<name>]` entries can now define
   Codex-style `extends` chains to those built-in profiles, and `command/exec`
   resolves the configured chain before choosing its sandbox. Configured
   `[permission_profiles.<name>.filesystem]` entries with `read` access are
   preserved as additional readable roots and now make custom read-only
   permission profiles use a strict read allow-list plus platform minimal
   runtime roots; entries with `write` or `read-write` access now compile into
   additional writable roots for `command/exec`, and `deny` entries compile
   into read/write deny rules that can override broader readable and writable
   roots.
   `[permission_profiles.<name>.network]`
   `enabled = true|false` now overrides the inherited built-in sandbox network
   default, Codex-style domain policy fields enforce through the managed
   command/exec network proxy, and Unix socket `allow` entries now materialize
   into macOS Seatbelt rules without enabling broad network access. Linux
   accepts the same Unix socket config for compatibility but cannot enforce
   path-level socket filters. Configured `:workspace_roots` /
   `:workspace_roots/<subpath>`
   filesystem entries now materialize against the owning thread's
   `runtimeWorkspaceRoots` before command execution, and TOML scoped
   filesystem tables such as
   `[permission_profiles.docs.filesystem.":workspace_roots"]` normalize into
   the same command sandbox roots. Configured `:tmpdir` / `:slash_tmp` entries
   now materialize to the current command environment's temp directory and
   `/tmp`, configured `:root` materializes to `/`, and configured `:minimal`
   materializes to platform default read roots needed by shell runtimes. Trailing
   `/**` filesystem entries now normalize to subtree roots, and bounded
   filesystem glob entries such as `*.env` or `docs/**/*.md` are expanded before
   command sandbox startup into concrete read, write, read-write, or deny roots.
   `glob_scan_max_depth` / `globScanMaxDepth` controls the bounded filesystem
   walk depth per profile, inheriting from extended profiles unless a child
   profile overrides it. Over-broad globs without a static parent directory are
   still rejected before scanning. Session-scoped `request_permissions` network
   domain grants now persist on server threads and feed later `command/exec`
   proxy policy; automatic ask-on-block now also applies when the effective
   network policy comes from an active thread permission profile, so callers do
   not need to repeat `permissionProfile` on every `command/exec` request.
5. **Protocol item stream:** Codex SDK emits `thread.started`, `turn.started`,
   `item.started/updated/completed`, and terminal turn events. Orca now keeps
   legacy JSONL names stable while the server adapter emits user steer
   `item_started` events plus agent-message `item_started`,
   `item_message_delta`, and `item_completed` lifecycle events. Tool call
   server streams now also emit command-execution `item_started` /
   `item_completed` events while preserving legacy `tool_requested` /
   `tool_completed` events. Reasoning streams now have a Codex-shaped
   `reasoning` item lifecycle with `summary` accumulation,
   `item_reasoning_delta`, and `item_completed`, while preserving legacy
   `reasoning_delta`. Structured `plan.updated` runtime events now surface as
   app-server-style `turn_plan_updated` notifications for update-plan tool
   changes. Codex plan-mode `<proposed_plan>` blocks are now split out of
   assistant message deltas into `plan` item lifecycle events with
   `item_plan_delta`, including split-tag and incomplete-block handling.
   Workflow runtime streams now emit `workflow` item lifecycle events with
   launch metadata, result summaries, and failed/completed terminal states while
   keeping legacy `workflow_*` events. Server JSONL submit now waits for
   background workflow observation so workflow item completion is testable and
   visible to clients. Edit and write-file tool calls now also emit
   Codex-schema `fileChange` item lifecycles with change path/kind,
   terminal status, output/error details, and preserved legacy tool events.
   These paths have writer-level, provider-mock, and server-mode contract
   coverage. Server resume now reopens the same thread id, resume/fork
   preserve stored permission snapshots when materializing live threads, and
   explicit resume/fork permission overrides are parsed, persisted, and exposed
   through thread summaries. Active server turns now use persisted `turnId`
   handles for control and event payloads, hook subprocess waits are
   cancel-aware for active turn interrupts, bash/external process waits observe
   active turn cancellation, TUI-local streaming bash can be interrupted
   without waiting for the shell timeout, and `shell/start` now supports
   explicit terminal mode plus initial PTY sizing. Active MCP tool waits now
   also observe turn cancellation, MCP stdio/SSE transports expose configurable
   startup/tool request timeouts, SSE timeout behavior has transport coverage,
   and app-server MCP failure payloads surface timeout details in legacy
   `tool_completed` events as well as model-visible tool results. Timed-out or
   disconnected MCP transports now refresh for subsequent tool calls without
   replaying the failed call. Shell start now reports requested/effective
   terminal modes and can fall back to pipe mode when PTY is unavailable.
   Server `shell/list` exposes active shell snapshots for client recovery,
   including description metadata that can be updated through `shell/update`.
   Codex-style `command/exec` / `command/exec/terminate` now provide an
   app-server-compatible command execution entrypoint over the same runtime
   shell-session manager, including buffered `cwd`, env override/unset, `tty`
   field parsing, invalid option validation, buffered/streaming output caps,
   streamed stdout/stderr output, stdin write compatibility, and PTY initial
   size/resize support.
   Background `turn/start` output now reuses the same stateful server writer as
   submit-mode output, and MCP calls now stream first-class `mcpToolCall`
   items with server/tool/arguments/result/error fields instead of being
   flattened into generic command items. Persisted thread turn/item projections
   now also merge assistant MCP tool calls with their tool-result messages into
   first-class `mcpToolCall` history items. TOML external tools now stream
   first-class `dynamicToolCall` items with tool/arguments/content/error
   fields in realtime server output, including an end-to-end server-mode
   contract through a real descriptor, and persisted thread turn/item
   projections merge external tool calls with results into
   `dynamicToolCall` history items. Stored tool-result messages now retain
   status/error/exit-code/truncation metadata for app-server history projection
   without changing the model-visible tool-result text, so failed, denied,
   not-implemented, or truncated tool calls are restored as first-class items
   without collapsing explicit non-success statuses into completed. TUI
   plan/subagent/approval events, workflow terminal notifications, workflow
   task-list/progress refreshes, interactive approval decisions, and
   `request_user_input` continuations now also flow through runtime
   event/handler boundaries. Realtime server item streaming now uses a shared
   `RuntimeEventProjector` reducer for assistant message, plan, reasoning,
   tool, file-change, and workflow item lifecycles instead of keeping those
   runtime-event state machines inside `ServerRequestWriter`. Realtime and
   persisted tool item projections now
   share MCP tool name parsing, JSON argument parsing, MCP/dynamic started-item
   builders, MCP result shaping, camelCase tool error object helpers, exit-code
   normalization from runtime payloads or persisted result metadata, and
   completed-status checks. Realtime
   MCP/dynamic tool item helpers also use the shared status check before emitting success
   result/content items, and realtime file-change item helpers now share the
   same success-output / error-detail split. Non-success output is surfaced as
   error detail without also being rendered as successful content.
   Command-execution items
   intentionally keep aggregated output for failed commands as diagnostic
   context, matching Codex `CommandExecution.aggregatedOutput`, and realtime
   command items now expose that field instead of an `output` alias. Public
   realtime and persisted app-server tool/file/command item types now use
   Codex-style camelCase names (`commandExecution`, `fileChange`,
   `mcpToolCall`, and `dynamicToolCall`) while keeping runtime event payload
   metadata stable. Realtime `fileChange` items now use Codex-style
   `inProgress` status, string `changes[].diff`, and no Orca-specific top-level
   `tool` / `output` / `error` fields; legacy `tool_completed` still carries
   diagnostic details for compatibility. Persisted bash tool calls now project as
   `commandExecution` history items with aggregated output/truncation metadata
   instead of Orca's generic `tool_call` shape, and those persisted command
   items now use shared projection helpers that preserve history-only metadata
   such as cwd/process/source/action/duration placeholders while keeping failed
   command aggregated output empty. Remaining persisted
   non-MCP/non-bash tool calls now use `dynamicToolCall` so public thread items
   no longer expose the legacy `tool_call` item type. Active steer injection
   now has server-mode coverage for multi-text input, proving both the
   user-item stream and the running model context preserve the full steered
   content. Package-3-style `task_list` / `task_stop` model tools now expose
   the runtime task registry directly, Codex-style app-server `approvalPolicy`
   aliases now flow through resume/fork/turn-start permission overrides, and
  package-3-style `permissionUpdates` now give server clients an incremental
  permission reducer for session-scoped rule/mode changes and additional
  working-directory roots, and Codex-style `activePermissionProfile` now
  persists through thread metadata and projects through `thread/read` /
  `thread/list`. Codex-style `request_permissions` is now model-visible and
  runtime-special: it accepts `permissions.fileSystem.read/write`,
  `permissions.network.enabled`, and permission-profile-style
  `permissions.network.domains`; it grants `fileSystem.write` roots as a
  turn-scoped overlay for later bash execution, deliberately avoids persisting
  those temporary roots into thread metadata, and server-mode `permission/respond`
  now completes the request / resolved round trip before continuing the turn.
  `session`-scoped permission grants now persist approved filesystem roots and
  network domain entries into thread metadata and live server thread state so
  later turns inherit the directory scope and later `command/exec` calls inherit
  the managed proxy allowlist/denylist. Codex-style `fileSystem.entries` with
  `read`, `write`, and
  `readWrite` access now normalizes into Orca read/write roots in both protocol
  `permission/respond` handling and model-visible `request_permissions`
  arguments. `strictAutoReview` now propagates through the permission response,
  `permission_resolved` server event, and model-visible tool output, then
  forces later approval-requiring tools in the same turn back through Ask even
  when the active mode is otherwise full-auto. Thread-bound server
  `shell/start` sessions can now share the owning thread's task registry, so
  model-visible `task_stop` can request a stop for the same shell task and
  later `shell/read` / `shell/list` reaps the process through the runtime
  shell-session kill path instead of only marking registry state.
  Package-3-style permission update `destination` now survives protocol
  decoding for mode/rule/directory updates, directory updates preserve their
  source through thread metadata, add-directory updates follow path-keyed
  replacement semantics, and remove-directory updates use the destination when
  applying Orca's persisted source metadata. Codex-style special filesystem
  entries now accept `project_roots` / legacy `current_working_directory`
  labels, normalize them to `:workspace_roots` paths at the protocol boundary,
  and materialize session-scope grants against runtime workspace roots before
  persisting additional working-directory metadata. Explicit Codex-style
  `runtimeWorkspaceRoots` thread/turn overrides now decode through the
  app-server protocol, persist in thread metadata, project through
  `thread/read` / `thread/list`, and rebind later `:workspace_roots` grants.
  TUI session picker/profile metadata now surfaces additional directory grants
  with Codex-style `:workspace_roots` labels instead of only materialized paths.
  Next, keep reducing remaining TUI/runtime protocol drift.

**Out of scope for P1:**

- Full app-server transport.
- Remote UI clients.
- Tool-system rewrite.
- Background shell/PTTY sessions.

### P2: Tool System Convergence

**Release target:** v0.1.33

**Current status:** runtime tool invocation preparation, approval request
construction, and hook-modified request validation flow through
`orca_runtime::tool_invocation` for normal controller execution, readonly
batches, subagent batches, and TUI approval prompts. Runtime tool dispatch now
routes through `RuntimeToolRouter`, keeping `ToolExecutionActor` focused on
invocation prep, approval, hooks, and result finalization while the router owns
workflow, subagent, task, permission, workflow IPC, and normal-tool routing.
Normal tool execution now delegates through `RuntimeNormalToolExecutor`, which
owns the shell-session bash branch and the MCP/external/built-in fallback path
outside `lifecycle.rs`; router-driven normal tools now pass grouped
`RuntimeNormalToolInvocation` state into lifecycle actors instead of calling
the long roots/cancel method directly. Historical projected tool completion now
funnels through shared `tool_item_projection::complete_projected_tool_item`, so
`thread_store/projection.rs` no longer owns MCP, dynamic, commandExecution, or
fileChange completed-item reconstruction.

**Goal:** reduce the remaining divergence between built-in tools, MCP tools,
external tools, approvals, and future plugin-provided tools.

**Scope:**

1. Normalize tool invocation records across all tool sources. Done in v0.1.33
   for built-in, MCP, and TOML external tools.
2. Move approval classification and validation result shaping into a shared
   runtime path. Done in v0.1.33.
3. Split runtime tool dispatch behind a focused router boundary. Done in
   v0.1.104.
4. Prepare for long-running shell sessions, worktree automation, and async
   subagents without adding them in the same patch. The normal-tool executor
   boundary landed in v0.1.105, and the injectable fallback boundary landed in
   v0.1.106; tool-call argument progress landed in v0.1.107,
   lifecycle-to-normal-tool invocation now funnels through a single
   runtime_normal_tool helper in v0.1.108, router-to-lifecycle normal-tool
   routing now uses a grouped `RuntimeNormalToolInvocation` in v0.1.109, and
   historical projected tool completion uses the shared
   `complete_projected_tool_item` helper in v0.1.110, and
   `ToolExecutionActor::handle_approval` now takes a grouped
   `ToolApprovalGateContext` in v0.1.111, and normal tool-turn execution now
   takes a grouped `RuntimeNormalToolTurnContext` in v0.1.112, and
   provider-to-tool-turn dispatch now takes a grouped `RuntimeToolTurnsContext`
   in v0.1.113, and filesystem sandbox-denial recovery now shares diagnostics
   and permission-request retry behavior across command/exec and model-visible
   bash in v0.1.114, and bash shell-session invocation now takes a grouped
   `RuntimeBashInvocationContext` in v0.1.115, runtime turn-loop
   orchestration moved from `lifecycle.rs` into `runtime_turn_loop` in
   v0.1.116, and runtime turn-iteration orchestration moved into
   `runtime_turn_iteration` in v0.1.117, runtime turn-opening orchestration moved into
   `runtime_turn_opening` in v0.1.118, runtime turn-start orchestration
   moved into `runtime_turn_start` in v0.1.119, runtime model-route
   orchestration moved into `runtime_model_route` in v0.1.120, runtime
   steer application moved into `runtime_steer` in v0.1.121, and runtime
   conversation bootstrap moved into `runtime_conversation_bootstrap` in
   v0.1.122, and runtime turn setup moved into `runtime_turn_setup` in
   v0.1.123, runtime lifecycle state machine types moved into
   `runtime_lifecycle` in v0.1.124, `RuntimeToolActorContext` moved into
   `runtime_tool_actor` in v0.1.125, and server command/exec active process
   state moved into `server/command_exec_manager.rs` in v0.1.126, server
   active-turn lifecycle state moved into `server/active_turn_manager.rs` in
   v0.1.127, server pending-permission request state moved into
   `server/permission_manager.rs` in v0.1.128, server shell-session state
   moved into `server/shell_manager.rs` in v0.1.129, and async subagent worker
   launch/completion ownership moved into `subagent_async_worker.rs` in
   v0.1.130, and readonly tool-turn batch execution moved into
   `runtime_readonly_tool_turn.rs` with grouped readonly contexts in
   v0.1.131. The feature work remains open after v0.1.131 for deeper
   reducer-style runtime convergence.

### P3: Shell Timeout Hardening

**Release target:** v0.1.37

**Current status:** synchronous shell and external tool execution now honor the
configurable `[tools].shell_timeout_secs` setting, default to 120 seconds, and
normalize values into the 1..3600 second range.

**Goal:** keep shell execution bounded without widening the PTY/session model in
the same patch.

**Scope:**

1. Add a shared child-process wait helper with timeout handling.
2. Thread the configured timeout from `RunConfig` into `orca-tools`.
3. Preserve current `bash` and external tool semantics for non-timeout cases.

**Verification:** covered by the release patch checks and the Rust checks for
`orca-core`, `orca-tools`, and `orca-runtime`.

### P4: History Store Boundary

**Release target:** v0.1.38

**Current status:** history/session persistence now flows through a dedicated
`SessionStore` boundary, with runtime session/controller call sites aligned to
the same entry point.

**Goal:** separate session history persistence from orchestration so the
runtime can evolve toward a Codex-style thread store without keeping
everything in one history module.

**Scope:**

1. Add a dedicated history store object that owns session list/load/archive/
   delete/search/compress helpers.
2. Route runtime session/controller code through the store instead of direct
   helper calls.
3. Keep the existing JSONL format and user-facing history commands stable.

**Verification:** Rust tests for `orca-runtime`, plus release staging and
public publish verification.

### P5: Claude Code Workflow Parity

**Release target:** v0.1.42

**Current status:** generated workflow drafts, draft edit/save/cancel actions,
launch from draft, saved workflow slash invocation, argument schema validation,
pause/resume/clone/restart controls, and evidence-bound final reporting are
implemented.

**Goal:** make workflow a first-class reviewable artifact rather than only a
JavaScript runner.

**Scope:**

1. Generate workflow drafts from model tool calls and expose preview metadata.
2. Let users edit, save, cancel, run, clone, pause, resume, and restart
   workflow runs through durable state.
3. Treat saved project/user workflows as reusable command-like assets.
4. Ground final workflow status and reports in evidence, verifier contracts,
   and child tool events.

**Verification:** workflow CLI/runtime/script/tool/host/event contract tests,
release staging, site build/SEO checks, and public publish verification.

### P6: Process Timeout Cleanup

**Release target:** v0.1.42

**Current status:** shell, external tools, hook commands, sandbox helpers, and
verifier commands now share non-interactive child process setup and timeout
cleanup behavior.

**Goal:** prevent timed-out commands from leaving descendant processes behind
while keeping existing command surfaces stable.

**Scope:**

1. Add shared non-interactive process preparation.
2. Terminate the full child process tree on timeout.
3. Apply the timeout behavior consistently across bash, external tools, hooks,
   sandboxed commands, and verifier execution.

### Skills And Plugins

**Release target:** after the TUI runtime protocol adapter and shell
session/PTTY releases.

**Goal:** evolve the existing Markdown skill loading into a plugin-compatible
instruction and capability system.

**Scope:**

- Keep current `list_skills`, `read_skill`, and explicit `$skill` injection
  stable.
- Add richer skill manifests only after protocol/tool boundaries can carry
  plugin-provided capabilities cleanly.

---

## Historical July 12 Priority Matrix (Superseded)

| Priority | Item | Why Now | Risk |
|----------|------|---------|------|
| P0.1 | Tool invocation closure | Prevents interrupted history from deleting completed context or repeating mutating side effects | Medium/High |
| P0.2 | One-shot operation cancellation and typed terminal outcome | Removes reset races and gives replacement, cancellation, failure, and abort distinct semantics | Medium |
| P0.3 | Runtime Operation Host and canonical turn executor | Gives one owner to async tasks, child scopes, joins, cleanup, events, and interactive session state across TUI/server/headless | High |
| P0.4 | Async provider through runtime | Removes the temporary per-call provider runtime and TUI double-worker path | Medium/High |
| P0.5 | Surface convergence | Moves server/headless first and TUI second onto the same runtime handles, then removes the TUI provider/tool kernel and direct execution dependencies | High |
| P1.1 | Semantic execution journal, one sequencer, and stable item ids | Makes canonical items, durable history, task state, goal state, and replay derive from one ordered source without synchronously journaling every token delta | High |
| P1.2 | Async ToolCallRuntime | Gives each invocation concurrency, approval, output, cancellation, cleanup, and exactly one truthful terminal outcome, including `indeterminate` after a crash | High |
| P1.3 | Durable interaction broker | Completed 2026-08-18 for Tool Approval, Permission Request, User Input, and MCP Elicitation across TUI, ACP, JSONL, and Headless | Done |
| P1.4 | Unified task supervisor, cancellation tree, lease, and fencing | Makes stop, pause, shutdown, reattach, stale-owner takeover, and stale-commit rejection verifiable | High |
| P2.1 | Checkpointable workflow and subagent resume | Resumes the same run from a safe cursor instead of replaying only completed cache entries | High |
| P2.2 | Runtime goal orchestrator | Complete in v0.2.52: Goal state, runs, outer turns, usage, leases, terminal verification, recovery, cancellation, and continuation policy are runtime-owned; the fixed continuation ceiling is removed, resumable interruption is distinct from blocking, and a durable progress watchdog guards repeated stalls | Done |
| P2.3 | App-server dependency inversion | Lets processors depend on operation/thread handles and stores instead of full mutable server state | Medium |
| P2.4 | Context and cache identity | Adds deterministic compatibility repair ids, immutable cache-critical prefixes, isolated fork state, and explicit checkpoints | Medium/High |
| P3 | Crate cleanup, plugins, remote compaction, worktree automation, richer PTY, multi-format reading | Remove source-text architecture tests and compatibility shims only after compiler-enforced ownership; build product breadth on stable contracts | Medium/High |

## Historical Priority Matrix

| Priority | Item | Why Now | Risk |
|----------|------|---------|------|
| P0 | Runtime-owned interactive session | Removes duplicated TUI/runtime state before deeper refactors | Medium |
| P0 | Published release verification | Prevents local tags from being mistaken for GitHub/npm releases | Low |
| P0 | Real API e2e release gate | Prevents local-only tests from being mistaken for provider/CLI/server readiness. Done in v0.1.34 | Low |
| P1 | Runtime protocol commands/events | Gives TUI/headless surfaces a shared contract | Medium |
| P1 | Runtime Task/Turn actor | Turn-start, model routing, pre/post model hooks, provider streaming, shell tool event shaping, pre/post tool hooks, non-interactive and interactive approval resolution, request-user-input handling, normal tool execution fallback, one tool actor context, runtime-special dispatch classification including `request_permissions`, workflow IPC execution, SubagentStatus execution, package-3-style `task_list` / `task_stop`, WorkflowDraft preview creation, workflow/subagent execution modules, active server-turn interrupt/resume, active steer item streaming/context injection including multi-text inputs, shell session/list/update controls, package-3-style incremental permission updates including additional directory roots, usage accounting, immutable turn-entry snapshotting through `RuntimeTurnContext`, read-only service grouping through `RuntimeTurnDeps`, mutable runtime handle grouping through `RuntimeTurnState`, execution/lifecycle grouping through `RuntimeTurnExecution`, lifecycle-owned agent-loop result shape and terminal constructors, runtime-lifecycle-owned task/turn state machine types, runtime-conversation-bootstrap-owned step composing session-owned bootstrap and initial history recording, lifecycle-owned runtime turn setup step composing context config, tool approval policy, and provider config construction, lifecycle-owned runtime turn opening step composing compaction, turn start, turn-start result folding, model routing, and steer application, lifecycle-owned runtime provider cycle step composing provider turn, provider turn result folding, provider error handling/result folding, and provider response/result folding, lifecycle-owned runtime turn iteration step composing turn opening, provider cycle execution, and provider-cycle result folding, runtime-turn-loop-owned iteration retry/return folding plus grouped input/executor objects to shrink the agent-loop call surface, lifecycle-owned runtime compaction step handling budget warning hooks, pre/post compact hooks, prompt-too-long reactive compaction, and history persistence, lifecycle-owned turn-start step handling first-turn prompt selection, turn start errors, and started event emission, lifecycle-owned turn-start result folding into continuation or agent-loop results, lifecycle-owned model-route step handling model routing, cost model updates, per-turn provider config selection, and `model.routed` event emission, lifecycle-owned provider-error step handling reactive prompt-too-long retry state, compaction retry decisions, and provider error failures, lifecycle-owned provider-error result folding into turn continuation, loop continuation, or agent-loop results, lifecycle-owned provider-turn result step handling response/terminal folding and cancelled-error event suppression, lifecycle-owned provider-turn result folding from response/failure outcomes into response continuation or agent-loop results, lifecycle-owned provider-turn step handling pre/post model hooks, provider streaming deltas, provider replay updates, provider error handling including prompt-too-long retry decisions, cancellation checks, usage accounting, and usage history persistence, lifecycle-owned provider-response step handling assistant response recording, provider turn terminal folding, provider tool request extraction, and tool-turn dispatch, lifecycle-owned provider-response result step folding continue/success/terminal outcomes into agent-loop results, runtime-steer-owned step draining multi-text inputs into conversation/history through grouped `RuntimeSteerInput`, tool-execution-owned approval policy construction, tool-execution-owned normal tool execution entrypoint, tool-invocation-owned provider tool schema override, tool-invocation-owned provider config construction, tool-invocation-owned provider tool request extraction, tool-invocation-owned child tool policy gate, tool-turn-owned cursor state, tool-turn outcome state, dispatch runner, normal/readonly tool-turn runners, read-only batch planning/execution/result recording, and normal result recording/status folding, subagent-execution-owned batch result recording and status folding, subagent-execution-owned batch tool-turn runner, controller-owned durable successful-root auto-memory enqueue, session-owned automatic-memory worker and exact-turn recovery, memory-owned typed candidate persistence and relevant recall, session-owned system prompt construction for agent conversation bootstrap, session-owned conversation bootstrap, session-owned initial history recording, session-owned assistant response recording, session-owned tool result recording for model content plus history persistence, and session-owned plan-state recording for conversation plus history persistence are seeded; next continue shrinking lifecycle/tool-turn call surfaces against the Codex/package 3 priority list | Medium/High |
| P1 | Storage-neutral ThreadStore | Codex keeps thread persistence behind a dedicated `thread-store` crate; Orca now exposes a `thread_store` module that owns the storage-neutral `ThreadStore` trait, the `JsonlThreadStore` backend type, the `ThreadStore` implementation, live thread handle, session metadata, summary, transcript, and writer API/behavior, JSONL record shape and stored-message conversion, append writing/redaction/locking, JSONL record reading/rewrite helpers, session metadata/transcript read models, thread-record lookup/path helpers, session list/load/read-summary/search/mutation operations, storage-neutral thread projection/page/filter types, message/turn/item projections, identified-record grouping with one isolated legacy id fallback, pagination, filters, and protocol-visible thread types, with `SessionStore` retained as a compatibility alias; live server message/turn/item/search projection, typed persisted turn/item identity, protocol thread shapes, session production wiring, agent-loop resume wiring, pagination, thread-record materialization, session list/load materialization, session search, delete/archive/rename/compress session mutations, and metadata/read/list/search/turn/item trait paths now go through the boundary without bridging projection helpers back through `history`. Append and read-modify-rewrite paths reopen the transcript after acquiring a stable sidecar lock, and the plaintext plus compressed names of one logical transcript resolve to that same cross-process lock; next consider a storage backend split only after the runtime/session protocol boundaries settle | Medium |
| P1 | Permission profiles and directory scope | Codex app-server has named active permission profiles, request-permissions approval round trips, `turn` / `session` grant scopes, filesystem entry semantics, special workspace-root labels, runtime workspace-root rebinding, and strict auto-review, while package 3 tracks update destinations, sources, and additional directories; Orca now has thread-scoped mode/rule snapshots, active permission profile metadata, built-in `permissionProfile` execution semantics for `command/exec`, configured profile `extends` chains plus filesystem `read` roots enforced as strict read allow-lists for custom read-only command sandbox profiles, filesystem `write` / `read-write` roots, filesystem `deny` read/write overrides, startup-time expansion of bounded configured filesystem globs for read/write/read-write/deny access, configurable glob scan depth with inherited profile defaults and child-profile overrides, `[network].enabled` command sandbox resolution, command/exec domain allow/deny policy enforcement through a managed loopback HTTP proxy with Codex-style denylist/allowlist block reasons plus normalized blocked-host attribution, default local/private literal blocking unless explicitly allowlisted, and DNS-resolved non-public target blocking before connect, session-scoped `request_permissions` network domain grants persisted on server threads and inherited by later thread-bound `command/exec` calls, session-scoped network deny overlays that override permission-profile allows while session allows cannot bypass existing profile denies, configured Unix socket allowlists materialized into macOS command/exec Seatbelt rules while non-macOS builds accept the config without path-level enforcement, configured `:workspace_roots` / `:workspace_roots/<subpath>` materialization against thread runtime roots, TOML scoped filesystem table normalization, trailing `/**` subtree normalization, configured `:tmpdir` / `:slash_tmp` materialization for command sandbox roots, configured `:root` materialization, configured `:minimal` platform-default read-root materialization, inherited thread active profiles for thread-bound command execution, incremental rule updates with destination metadata, persisted additional working directories with source-aware replacement/removal, protocol projections, bash sandbox roots, turn-scoped `request_permissions` write-root overlays, server-mediated permission approvals, session-scope grant persistence, Codex-style `fileSystem.entries` normalization including `project_roots` / `:workspace_roots` special paths, explicit `runtimeWorkspaceRoots` thread/turn overrides, TUI session-picker labels for workspace-root-scoped directory grants, `strictAutoReview` propagation that re-prompts later same-turn tools, thread-bound shell tasks that can be stopped through model-visible `task_stop`, and inherited-profile network blocks that now route through the existing permission grant/retry path; next reduce remaining TUI/runtime protocol drift | Medium |
| P1 | TUI event and interaction adapters | At this historical checkpoint, assistant deltas, usage, model routing notices, errors, session completion, tool requested/completed, plan updated, subagent started/completed, approval prompts/resolution notices, request-user-input prompts/results, verification started/completed notices, workflow terminal notifications, workflow lifecycle notices, and workflow task-list/progress refreshes flowed through runtime `EventFactory` and typed runtime-surface boundaries; ordinary TUI turns no longer installed a local interaction adapter or pending store, while `RuntimePendingInteractionStore` still remained as a documented source-compatibility projection. That projection was deleted on 2026-08-18 after durable broker completion. The hosted action receive/lifecycle controller had one focused owner outside the renderer, renderer runtime-event plus frame timing/presentation coordination had focused owners, terminal session startup was a focused pending-to-activated owner, and renderer input wake/suspend, semantic input routing, interaction-acknowledgement draining, the bounded runtime-event inbox and pre-agent close, typed input-vs-runtime iteration routing, the complete foreground renderer cycle, active-terminal initial-title/first-frame bootstrap, receipt-backed recorded terminal-task import, and non-replayable safe Running MainSession adoption had focused owners; the next work at that checkpoint was queued/paused/stopping, approval, failed/retryable, and rich-task cold reconstruction with durable operation, interaction, and ownership fences | Medium |
| P2 | Unified tool invocation records | First-class MCP and external/dynamic app-server stream and history items are seeded, including failed/denied/not-implemented status plus error/exit-code/truncation restoration in history projections and legacy realtime `tool_completed` exit-code/result-kind preservation; MCP resource list/read/template tools now share the registry path, all-server resource/template discovery surfaces registry startup failures plus per-server list failures, and resource-capability caching avoids probing tools-only servers during all-server discovery; next reduce remaining TUI/runtime protocol drift | Medium |
| P2 | Shared approval/result shaping | Historical first-class tool item completion preserves explicit non-success statuses, realtime MCP/dynamic/file-change item helpers avoid success result/content payloads for non-completed statuses, realtime MCP/dynamic item errors now carry `exitCode` when tool completion reports one, command-execution items keep failed-command output as diagnostic aggregated output by contract, realtime/persisted tool item projections now share MCP parsing/started/result/error/status helpers, and `ToolExecutionActor::handle_approval` receives one grouped `ToolApprovalGateContext`; next continue moving approval/result shaping helpers behind focused runtime-owned context boundaries | Medium |
| Skills | Plugin-compatible skill manifests | Unlocks reusable instruction bundles after runtime contracts stabilize | Medium |
| Later | Cross-platform PTY depth | Shell session/list/update, command/exec compatibility, and shell output/exited stream notifications are seeded; deeper Windows PTY and richer terminal fidelity remain larger runtime work | High |
| Later | Remote compaction | High value, model-dependent behavior | Medium/High |
| Later | Worktree automation | High value, more filesystem/git risk | High |
| Later | Multi-format reading | Useful, but dependency and rendering heavy | Medium |

---

## Technical Decisions

| Decision | Current Choice | Notes |
|----------|----------------|-------|
| Tokenizer | `tiktoken-rs` BPE | Good enough for DeepSeek-compatible accounting until a DeepSeek-specific tokenizer is required |
| Config format | TOML | Keep user-facing config stable |
| Tool registry | `ToolSpec` capability registry | All built-ins, MCP, and external tools should flow through this path |
| Default truncation | Byte/token policy with compatibility defaults | Keep result budgets consistent as tool execution centralizes |
| MCP transport | stdio and SSE | Keep routing namespaced as `mcp__server__tool`; `startup_timeout_ms` and `tool_timeout_ms` bound startup/tool/resource requests, resource reads stay read-only, and failed transports refresh for later calls |
| Sandbox | macOS Seatbelt first, graceful fallback elsewhere | Add summaries before adding more platform sandboxes |
| History | JSONL transcript files | Runtime now owns interactive writer setup; introduce ThreadStore trait before considering SQLite metadata |
| Interactive session | `orca_runtime::session::InteractiveSession` plus `orca_runtime::lifecycle`/`RuntimeTaskActor` seed | TUI wrapper and shell/tool events now carry lifecycle metadata, but remain temporary while protocol/events and task/turn actor ownership are extracted |
| Skills | Markdown `SKILL.md` files | Keep instruction loading stable before adding plugin-provided capabilities |

---

## Completion Gates

Every patch phase must satisfy:

1. Version references are aligned across `Cargo.toml`, `Cargo.lock`, README, website metadata, and release notes.
2. Tests relevant to the touched surface pass fresh.
3. Release staging still validates with the current version.
4. `node scripts/release/real-api-e2e.mjs` passes with a real DeepSeek API key before tagging.
5. `git diff --check` is clean.
6. The release note describes user-visible changes and follow-up scope.
