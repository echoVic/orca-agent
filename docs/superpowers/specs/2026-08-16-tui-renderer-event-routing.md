# TUI Renderer Iteration Event Routing Ownership

Status: Implemented

## Context And Classification

At audited base `61950c85d`, `app.rs` is 7,778 lines. Focused owners already
exist for frame scheduling, input wake, semantic input routing, runtime-event
inbox lifetime, single runtime-event effects, and interaction
acknowledgements. The outer renderer loop still owns one boundary directly:
the `IterationEvent` branch that selects the input router or runtime-event
owner and translates their results into the frame scheduler's
`io::Result<Option<i32>>` contract.

This is a boundary defect, not a local user-visible bug. The branch currently
binds every input/runtime collaborator inside `app.rs`, so future changes can
accidentally translate an input error, drop an exit code, or let a runtime
event fabricate an exit even though the lower owners are already focused and
tested.

## TUI Value And Independent Slice

The user-facing reliability value is one typed and directly tested boundary
between frame scheduling and semantic event handling. The slice independently
proves that input exits and terminal errors still escape the loop exactly,
while runtime events mutate presentation through the admitted-event owner and
never invent a renderer exit.

It does not move the surrounding loop, create a second reducer, or retain any
event. The old inline branch is deleted in the same commit, so this is a final
owner boundary rather than a compatibility layer.

## Decision

Add `renderer_event_router.rs` with one private `RendererIterationEventRouter`.
It borrows the same state, config, shared config, action sender, preloaded
transcript, composer, Vim state, theme, presentation, initial prompt,
workflow-notification queue, and `RendererRuntimeEventOwner` already captured
by the inline closure.

Its consuming `route` method accepts one typed
`IterationEvent<BatchedInputEvent, TuiEvent>`, the existing event timestamp,
and the existing terminal-clear callback:

- `Input` constructs the existing `RendererInputRouter` with the same
  collaborators and returns its `io::Result<Option<i32>>` unchanged.
- `Runtime` delegates once to `RendererRuntimeEventOwner::handle` and returns
  `Ok(None)`.

`app.rs` keeps the frame iteration and supplies only a fresh router per event.
No event clone, queue, buffer, thread, retry, catch, or state is added.

## Frozen Behavior

### Normal Routing

1. `RendererFrameOwner` still drains coalesced input before runtime events and
   keeps the same `usize::MAX` input limit and 256 runtime-event limit.
2. Each input event reaches `RendererInputRouter` exactly once with the same
   event timestamp and clear-terminal callback.
3. `Ok(Some(code))`, `Ok(None)`, and every `io::Error` from input routing are
   returned without translation. The frame scheduler therefore keeps its
   immediate exit and error short-circuit behavior.
4. Each runtime event reaches `RendererRuntimeEventOwner::handle` exactly once
   with the same collaborators. The branch returns `Ok(None)` after the
   existing owner finishes.
5. Composer/mention synchronization still runs after a successful mixed
   iteration and before app exit-code handling; frame presentation remains
   after the exit check.

### Cancellation, Rejection, Timeout, Retry, And Disconnect

1. Keyboard cancellation and exit stay in `RendererInputRouter` and its
   lower typed action owners. A second idle Ctrl+C still returns exit 130 and
   dispatches the same `UserAction::Cancel`.
2. Runtime cancellation, rejection, timeout, retry, approval, and user-input
   events stay in `RendererRuntimeEventOwner` plus `runtime_event_actions`.
   This router does not interpret their variants or terminal status.
3. Input-runtime disconnect errors remain owned by `RendererInputWakeOwner`;
   runtime inbox disconnect remains a non-blocking empty iterator;
   agent/controller disconnect and shutdown remain outside this router.
4. A terminal-clear failure from an input shortcut remains the exact original
   `io::Error`; it is not converted into a TUI message or exit code.

### Restart And Persistence

1. The router owns no durable or resumable state. Restart reconstructs it from
   the same process-local collaborators on each event.
2. Session history, surface projections, pending interactions, operation
   recovery, retry counters, and runtime snapshots remain unchanged and keep
   their existing owners.
3. No CLI, TUI event/action, server/JSONL, app-server, ACP, public Rust API,
   history, persistence, or schema format changes.

## Ownership, Lifetime, And Rollback

- `renderer_frame.rs` owns event ordering, bounds, dirty scheduling, and exit
  short-circuit admission.
- `renderer_event_router.rs` owns only the typed input-vs-runtime branch and
  exact result delegation for one event.
- `renderer_input_router.rs` owns semantic input preprocessing, status
  dispatch, cancellation, clear errors, and input exit codes.
- `renderer_runtime.rs` owns attachment admission, special runtime events,
  deferred prompt submission, mention state, and admitted-event effects.
- Input receivers, runtime inbox, acknowledgement receiver, agent/controller
  threads, cancellation handles, joins, terminal resources, and persistent
  state remain with their current owners.
- Migration is atomic: add the owner, route the production closure through it,
  and delete the old match in one commit. Rollback restores that inline match
  and removes the private module; no data migration or cleanup is required.

## Validator Contract

Add one closed `renderer_event_routing` TUI entrypoint with path-specific app
and owner anchors. Migrate the existing `renderer_input_routing` and
`renderer_runtime_events` production-delegation anchors from `app.rs` to the
new router without weakening their lower-owner anchors. Negative self-tests
must prove imports, lower-owner calls/tests, the `IterationEvent` enum, or new
owner tests cannot mask deletion of app construction/delegation, either typed
branch, or the runtime branch's `Ok(None)` result. The existing lower-owner
self-tests continue to protect exact input and runtime delegation.

## Test Strategy

1. Register a module containing direct tests that import the absent owner and
   require RED with unresolved `RendererIterationEventRouter`.
2. Prove a runtime `Notice` is applied through the real runtime-event owner,
   returns `None`, and never calls the terminal-clear callback.
3. Prove a second idle Ctrl+C routes through the real input owner, returns
   exact exit 130, and dispatches `UserAction::Cancel`.
4. Prove Ctrl+L returns the exact clear-terminal `io::Error` without
   translation.
5. Keep frame input-before-runtime/exit tests, input-router semantic tests,
   runtime-event owner tests, and full PTY behavior as downstream evidence.
6. Run compiler checks, both validator families and self-tests, formatter,
   diff check, full serial TUI, and PTY before and after rebase and again on
   integrated local `main`.

## Acceptance Criteria

1. `run_tui_inner` no longer directly matches `IterationEvent` or constructs
   the lower input/runtime branch behavior inline.
2. One private router preserves exact collaborators, timestamp, terminal-clear
   callback, input exit/error result, and runtime `Ok(None)` behavior.
3. Frame ordering/limits, composer sync, exit check, presentation, shutdown,
   cancellation, disconnect, and persistence semantics remain unchanged.
4. Direct owner tests are RED before implementation and GREEN afterward.
5. Closed validator anchors and deletion self-tests cover production paths
   without broadening mutation or harmless-method baselines.
6. Full TUI and PTY behavior and all external contracts remain compatible;
   independent review has no unresolved Critical or Important finding.
7. After local-main integration and root verification, remove only this slice
   worktree and merged topic branch immediately.

## Evidence

- `git fetch origin` confirms `origin/main` is not ahead; local `main` is 36
  linear commits ahead and clean. `.worktrees` is ignored by `.gitignore`.
- Base `61950c85d` passes the runtime-inbox owner 3/3, full serial TUI
  1,139/1,139, PTY 6/6, runtime and Windows validators/self-tests, formatter,
  manifest digest, and diff checks.
- The audited branch is `app.rs:328-364`. `renderer_frame.rs:76-98` already
  owns mixed scheduling, `renderer_input_router.rs:27-180` already owns input
  behavior, and `renderer_runtime.rs:18-142` already owns runtime-event
  behavior.
- Base source sizes are `app.rs` 7,778 lines,
  `renderer_input_router.rs` 461 lines, `renderer_runtime.rs` 395 lines, and
  `renderer_frame.rs` 525 lines.
- RED failed only with unresolved import of `RendererIterationEventRouter`.
  GREEN passes the three direct owner tests plus seven input-router, two
  runtime-owner, and six frame-owner tests.
- The implemented source is `app.rs` 7,759 lines and
  `renderer_event_router.rs` 273 lines. The closed runtime validator and its
  deletion self-tests pass with lower input/runtime anchors migrated to the
  new owner.
- Ordinary and tests compiler checks, runtime and Windows validators plus
  self-tests, formatter, manifest digest, and diff checks pass. The first full
  serial TUI gate passes 1,142/1,142 and the root PTY contract passes 6/6.
- CodeRabbit reviewed all 11 staged slice files with zero findings; manual
  review found no Critical or Important ownership, ordering, lifecycle, or
  compatibility defect.
- The semantic commit was rebased onto the latest local `main`, fast-forwarded
  into local `main`, and reverified there with the owner 3/3,
  both validator families and self-tests, full serial TUI 1,142/1,142, and
  PTY 6/6. The dedicated worktree and topic branch were then removed while
  unrelated worktrees remained intact.

## Out Of Scope

- Moving frame scheduling, input coalescing, event inboxes, acknowledgement
  draining, composer synchronization, presentation, terminal cleanup, or
  hosted-agent shutdown.
- Changing any lower input shortcut, runtime event, cancellation, retry,
  timeout, interaction, attachment, or disconnect policy.
- Adding buffering, cloning, retries, catches, persistent state, or a second
  event reducer.
- Cold legacy registry reconciliation, provider, runtime protocol,
  persistence, server, JSONL, ACP, shell, or release work.
