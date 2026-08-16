# TUI Renderer Runtime Inbox Ownership

Status: Implemented

## Context

At audited base `6fd191f11`, `app.rs` is 7,777 lines. Runtime events already
use a bounded `TuiEvent` channel, `RendererFrameOwner` already owns mixed
input/runtime iteration scheduling, and `RendererRuntimeEventOwner` already
owns attachment admission and the effect of one admitted event. The outer
renderer loop still owns the receive-side protocol directly:

1. retain the exact `TuiEventReceiver` returned by `tui_event_channel`;
2. expose its non-blocking `try_iter()` to the frame iteration;
3. keep input processing before runtime-event processing and cap runtime work
   at `TUI_EVENT_CAPACITY`; and
4. after terminal cleanup, stop mention search, drop the event receiver to
   release any capacity-blocked producer, then shut down the hosted agent.

The receiver identity and close boundary are one lifecycle responsibility.
Leaving them as a raw app local makes the outer loop responsible for channel
semantics that already have focused scheduling and event-reduction owners.

## Decision

Add `renderer_runtime_inbox.rs` with one private
`RendererRuntimeInboxOwner`. It consumes the existing `TuiEventReceiver`,
exposes a borrowed non-blocking `pending()` iterator, and has a consuming
`shutdown()` that explicitly drops the receiver.

`app.rs` constructs the owner from `pending_event_rx`, passes
`owner.pending()` to the existing `RendererFrameOwner::run_iteration`, and
calls `owner.shutdown()` after `RendererRuntimeEventOwner::shutdown` but
before `TuiAgentRuntime::shutdown`.

This extraction adds no channel, receiver clone, sender clone, capacity,
thread, blocking wait, retry, wakeup, event, or scheduling layer.

## Frozen Semantics

### Inbox And Iteration

1. `tui_event_channel()` remains the only renderer runtime-event mailbox and
   keeps its bounded capacity of `TUI_EVENT_CAPACITY` (256).
2. The owner consumes exactly the receiver returned by that channel. It does
   not clone or replace it.
3. `pending()` returns the existing crossbeam `try_iter()` behavior: it never
   blocks, yields events in channel FIFO order, and stops when the receiver is
   currently empty or disconnected.
4. Dropping a partially consumed iterator leaves unread queued events in the
   receiver for a later call, matching the existing borrowed iterator.
5. `RendererFrameOwner` still processes input before runtime events, marks the
   frame dirty for each admitted iteration event, stops immediately on an
   input exit, and caps runtime work at `MAX_RUNTIME_EVENTS_PER_BATCH ==
   TUI_EVENT_CAPACITY`.
6. `RendererRuntimeEventOwner` remains the only renderer-side attachment
   admission, special-event, deferred prompt, mention synchronization, and
   single-event reduction owner.

### Shutdown

1. Terminal presentation cleanup still completes before runtime-side
   renderer resources begin shutting down.
2. `RendererRuntimeEventOwner::shutdown` still stops and joins mention search
   before the inbox closes, so that producer is not abandoned against a
   disconnected channel.
3. `RendererRuntimeInboxOwner::shutdown` then drops the receiver. A producer
   blocked because the bounded mailbox is full is released with a send error.
4. `TuiAgentRuntime::shutdown` remains last. It keeps cancellation, dispatcher
   shutdown, hosted-controller join, acknowledgement-lane close, and error
   propagation unchanged.
5. Normal drop without `shutdown()` still drops the receiver through Rust
   ownership; the explicit production call documents and validates the
   required ordering.

## Ownership And Compatibility

- `channels.rs` keeps mailbox types, capacity, construction, and generic
  bounded-channel tests.
- `renderer_runtime_inbox.rs` owns the renderer runtime-event receiver,
  borrowed non-blocking FIFO iteration, and explicit close boundary.
- `renderer_frame.rs` keeps input/runtime priority, batch limits, dirty
  scheduling, and draw admission.
- `renderer_runtime.rs` keeps single-event admission/reduction, mention
  synchronization, deferred prompt state, and mention-worker shutdown.
- `agent_runtime.rs` keeps dispatcher/controller lifetime and hosted-agent
  shutdown.
- `app.rs` keeps construction, owner ordering, event routing collaborators,
  terminal lifecycle, and final exit projection.
- No `TuiEvent`, `UserAction`, runtime surface, public Rust API, CLI/slash
  syntax, server/JSONL, app-server, ACP, history, persistence, schema, error
  text, or visible TUI behavior changes.

## Validator Contract

Add one closed `renderer_runtime_inbox` TUI entrypoint with path-specific app
and owner anchors. Negative self-tests must prove that imports, channel helper
tests, frame-scheduler tests, event-reducer tests, or owner unit tests cannot
mask deletion of production owner construction, pending-iterator delegation,
receiver retention, explicit receiver drop, or the required
runtime-owner-before-inbox-before-agent shutdown sequence.

## Test Strategy

1. Register a module containing direct tests that import the absent owner and
   require RED with unresolved `RendererRuntimeInboxOwner`.
2. Prove connected-empty and disconnected-empty pending iterators are inert.
3. Prove post-construction sends are yielded FIFO, a partially consumed
   iterator leaves the remaining event queued, and a later call receives it.
4. Fill the production bounded mailbox, block one producer send, call owner
   shutdown, and prove the producer is released with a disconnect error.
5. Keep channel capacity/drop tests, frame input/runtime ordering and bounds,
   runtime-event reducer tests, and agent shutdown tests as downstream
   evidence.
6. Run compiler checks, both validator families and self-tests, formatter,
   diff check, full serial TUI, and PTY before and after rebase and again on
   integrated local `main`.

## Acceptance Criteria

1. `run_tui_inner` no longer stores a raw runtime-event receiver or directly
   calls `try_iter()`/`drop` on it.
2. One private owner preserves exact receiver identity, non-blocking borrowed
   FIFO iteration, partial-consumption behavior, and explicit close.
3. App preserves terminal cleanup, mention shutdown, inbox close, and agent
   shutdown order exactly.
4. Direct owner tests are RED before implementation and GREEN afterward.
5. Closed validator anchors and deletion self-tests cover production paths
   without broadening mutation or harmless-method baselines.
6. Full TUI and PTY behavior and all public/protocol/persistence contracts
   remain compatible; independent review has no unresolved Critical or
   Important finding.
7. After local-main integration and root verification, remove only this slice
   worktree and merged topic branch immediately.

## Evidence

- Base `6fd191f11` passes the renderer interaction-ack owner suite 4/4, full
  serial TUI 1,136/1,136, PTY 6/6, runtime and Windows validators/self-tests,
  formatter, digest, and diff checks.
- The audited raw receiver path is `app.rs:150,252,323,404`.
  `renderer_frame.rs:76-98` already owns bounded mixed iteration;
  `renderer_runtime.rs:18-142` already owns admitted event effects and mention
  shutdown; `channels.rs:5-14,28-113` owns capacity, construction, FIFO
  delivery, backpressure, and receiver-drop release tests.
- Base source sizes are `app.rs` 7,777 lines, `renderer_runtime.rs` 395 lines,
  `renderer_frame.rs` 525 lines, `channels.rs` 172 lines, and `agent_runtime.rs`
  262 lines.
- The RED owner suite failed only with unresolved
  `RendererRuntimeInboxOwner`; after implementation its three direct tests
  pass. They prove inert empty/disconnected iteration, FIFO and partial
  consumption, post-construction receiver identity, and full-mailbox producer
  release on explicit shutdown.
- Focused downstream suites pass: mailbox/channel coverage 6/6,
  `RendererFrameOwner` 6/6, `RendererRuntimeEventOwner` 2/2, and
  `TuiAgentRuntime` 3/3.
- The runtime contract validator and its deletion self-tests pass with one new
  closed entrypoint. App construction/pending/shutdown-order paths and owner
  receiver/construction/`try_iter`/explicit-drop paths cannot be masked by
  imports, generic channel tests, or owner tests.
- Ordinary and test compiler checks, runtime and Windows validators plus their
  deletion self-tests, formatter, manifest digest equality, and diff checks
  pass. The closed Rust entrypoint mirror is 38 entries; the broader roadmap
  owner/boundary inventory is 40.
- The first full serial TUI gate passes 1,139/1,139 and the PTY contract passes
  6/6.
- CodeRabbit reviewed all 11 staged slice files and reported zero findings.
- The slice is recorded as one semantic commit, initially `cc49d1042` before
  evidence-only amendment and local-main rebase.
- Local `main` remained at base `6fd191f11`; `git rebase main` reported the
  topic up to date. Post-rebase owner 3/3, focused downstream 17/17, runtime
  and Windows validators/self-tests, formatter, diff check, full serial TUI
  1,139/1,139, and PTY 6/6 all pass.
- Local `main` fast-forwarded to the single slice commit. Root verification
  passes owner 3/3, runtime and Windows validators/self-tests, formatter, diff
  check, full serial TUI 1,139/1,139, and PTY 6/6. The clean, merged slice
  worktree and topic branch were then removed immediately; unrelated
  worktrees remain intact.
- Implemented source sizes are `app.rs` 7,778 lines and
  `renderer_runtime_inbox.rs` 104 lines; the existing event reducer, frame,
  channel, and agent-runtime owners are unchanged.

## Out Of Scope

- Changing event/action payloads, channel capacity, sender behavior,
  backpressure, event coalescing, input priority, runtime-event batch limits,
  or frame dirtiness.
- Moving attachment admission, event reduction, mention search, terminal
  presentation, input wake/routing, interaction acknowledgements, hosted
  cancellation, or exit-session selection.
- Adding a wakeup channel, blocking receive, event buffering, or receiver
  clone.
- Cold legacy registry reconciliation, provider, runtime protocol,
  persistence, server, JSONL, ACP, shell, or release work.
