# TUI Renderer Loop Ownership

Status: Implemented

## Context And Classification

At audited base `1b67ac53d`, `app.rs` is 7,759 lines. Focused owners now
cover terminal startup, frame scheduling/presentation, input wake, semantic
input routing, runtime-event admission/effects, runtime-inbox lifetime,
interaction acknowledgements, and typed input-vs-runtime event routing.
`run_tui_inner` still directly owns the remaining 76-line renderer iteration
cycle at `app.rs:295-370`.

That cycle is an architecture boundary, not a new user-visible bug. Its exact
order determines whether an expired Vim insert-escape sequence is flushed
before polling, whether resume redraw precedes later events, whether
acknowledgements dirty the same frame, whether composer/mention state is
reconciled after dispatch, and whether an input exit bypasses presentation.
Leaving the sequence as a wide app closure makes those cross-owner invariants
hard to test as one unit.

## TUI Value And Independent Slice

The user-facing reliability value is one directly tested renderer-cycle owner
that preserves input responsiveness, runtime event visibility, exact exit and
error propagation, and frame ordering. The slice removes the whole inline
loop in the same commit; it does not add a compatibility layer or second
reducer.

## Decision

Add `renderer_loop.rs` with one private `RendererLoopOwner`. It owns a
`RendererFrameOwner` and borrows the existing input-wake, acknowledgement,
runtime-inbox, runtime-event, state, config, composer, presentation, and
workspace collaborators for the lifetime of one foreground renderer run.

Its consuming `run` method keeps the existing loop and returns the exact input
exit code. It is generic over the existing ratatui `Backend` boundary and
accepts the existing terminal-clear, clipboard-copy, and pending-presentation
callbacks. Production passes the same functions and closures as today.

Generalize `RendererFrameOwner::resume` and
`presentation::resume_terminal_render` from the concrete inline backend to
the same `Terminal<B>`/`Backend` constraint already used by frame
presentation. This is a crate-private type generalization only; the
production backend and behavior do not change. It permits direct owner tests
with `TestBackend` instead of writing terminal escapes to process stdout.

## Frozen Behavior

### Normal Iteration Order

Every iteration preserves this exact order:

1. Capture `Instant::now()`.
2. Flush an expired insert-escape sequence. Mark the frame dirty only when the
   flush mutates composer/Vim/input state.
3. Prepare frame animation, copy-notice, edit-highlight, and poll timeout.
4. Receive prioritized input/control. A resume clears the terminal,
   invalidates presentation title state, and marks the frame dirty before
   returning an empty input batch.
5. Drain interaction acknowledgements FIFO and mark the frame dirty once for
   a non-empty batch.
6. Coalesce adjacent wheel input with the existing distance `3`, process all
   input before at most 256 runtime events, and route each typed event through
   `RendererIterationEventRouter` with the existing timestamp and terminal
   clear callback.
7. After successful mixed dispatch, reconcile mention roots, mention
   bindings, atomic skill tokens, and mention search against the current
   composer.
8. If the iteration returns an exit code, return it immediately without
   clipboard consumption, pending terminal output, or drawing.
9. Otherwise consume a pending clipboard copy, write pending terminal output,
   and draw only at the frame owner's admitted timestamp.

The initial title/draw still happens before constructing the loop owner.
Terminal cleanup still happens after it returns.

### Cancellation, Rejection, Timeout, Retry, And Disconnect

1. Keyboard cancellation and exit remain in the input router and lower action
   owners. The second idle Ctrl+C still sends `UserAction::Cancel`, returns
   130, performs post-dispatch composer synchronization, and skips
   presentation.
2. Runtime cancellation, rejection, timeout, retry, approval, and user-input
   events remain in `RendererRuntimeEventOwner` and
   `runtime_event_actions`. The loop does not interpret variants or fabricate
   terminal state.
3. Input timeout remains an empty batch. Suspend acknowledgement, repeated
   suspend, resume, and exact input disconnect errors remain in
   `RendererInputWakeOwner`.
4. Runtime-inbox disconnection remains an empty non-blocking iterator. The
   256-event runtime bound, input-first ordering, and dirty-frame admission
   remain in `RendererFrameOwner`/`frame_scheduler`.
5. Resume, terminal-clear, event-routing, and draw errors propagate as the
   exact original `io::Error`. No catch, retry, message conversion, or exit
   translation is added.

### Lifecycle, Restart, And Persistence

1. `RendererLoopOwner` owns no thread, channel endpoint, cancellation token,
   session identity, or durable state. It drops only borrowed references and
   its process-local frame scheduler when the foreground loop exits.
2. The app retains shutdown order: terminal cleanup, mention-worker shutdown,
   runtime-inbox receiver close, then hosted-agent cancellation/join.
3. Restart reconstructs the loop from the same runtime snapshot and local
   collaborators. History, surface projections, pending interactions,
   operation recovery, retry counters, and runtime snapshots are unchanged.
4. No CLI, TUI action/event, server/JSONL, app-server, ACP, public Rust API,
   persistence, history, or schema format changes.

## Ownership And Rollback

- `renderer_loop.rs` owns only the full foreground iteration sequence and its
  exit-before-presentation rule.
- `renderer_frame.rs` retains frame timing, animation, dirty state, batch
  admission, draw timestamp, clipboard consumption, and drawing.
- `renderer_event_router.rs`, `renderer_input_router.rs`, and
  `renderer_runtime.rs` retain typed branch, semantic input, and runtime-event
  policies respectively.
- Input, runtime, and acknowledgement receivers retain their existing owners.
  The app retains creation plus shutdown/cleanup ordering.
- Rollback restores the inline loop and concrete resume signatures; there is
  no data migration or cleanup.

## Validator Contract

Add one closed `renderer_loop` TUI entrypoint with path-specific app and owner
anchors. Migrate existing frame, input-wake, interaction-acknowledgement,
runtime-inbox, and event-router production-delegation anchors from the inline
app loop to `renderer_loop.rs` without weakening their focused owner anchors.

Negative self-tests must prove imports, constructors, lower-owner calls, or
owner tests cannot mask deletion of app construction/run, expired-escape
flush, prepare-before-receive, resume delegation, acknowledgement dirtying,
mixed dispatch, composer synchronization, exit-before-presentation, or
presentation.

## Test Strategy

1. Register the new module and direct tests importing the absent
   `RendererLoopOwner`; RED must fail only because the owner is unresolved.
2. With real input channels and `TestBackend`, prove a second idle Ctrl+C
   sends Cancel, returns 130, and invokes no pending-presentation callback.
3. Queue an input-control `Resumed`, one runtime Notice, and a later Ctrl+C;
   prove the runtime event is reduced and presented exactly once before the
   next input exit.
4. Prove Ctrl+L returns the exact clear-terminal error without presentation.
5. Keep the event-router, frame, input-wake, acknowledgement, runtime-inbox,
   runtime-event, input-router, full TUI, and PTY suites as downstream
   evidence.
6. Run compiler checks, both validator families and self-tests, formatter,
   digest equality, diff check, full serial TUI, and PTY before and after
   rebase and again on integrated local `main`.

## Acceptance Criteria

1. `run_tui_inner` no longer contains the foreground renderer iteration loop.
2. One private owner preserves the exact order, collaborators, limits,
   timestamps, error/exit propagation, and exit-before-presentation behavior.
3. Terminal initialization/cleanup and runtime/inbox/agent shutdown remain in
   `app.rs` with unchanged order.
4. Direct owner tests are RED before implementation and GREEN afterward.
5. Closed validator anchors and deletion self-tests cover production paths
   without broadening mutation or harmless-method baselines.
6. Full TUI and PTY behavior and external contracts remain compatible;
   independent review has no unresolved Critical or Important finding.
7. After local-main integration and root verification, remove only this
   slice's worktree and topic branch immediately.

## Evidence

- Base `1b67ac53d` is clean and 37 linear commits ahead of `origin/main`; the
  remote is not ahead and `.worktrees` remains ignored.
- The preceding renderer-event-routing owner passes 3/3, its affected owners
  15/15, the full serial TUI 1,142/1,142, PTY 6/6, both validator families and
  self-tests, formatter, digest, and diff checks.
- The audited inline cycle is `app.rs:295-370`. Focused lower owners are
  `renderer_frame.rs` 525 lines, `renderer_input_wake.rs` 298 lines,
  `renderer_interaction_acks.rs` 182 lines, `renderer_event_router.rs` 273
  lines, `renderer_runtime.rs` 395 lines, and `renderer_runtime_inbox.rs` 104
  lines.
- The direct RED command failed only with `E0432` because
  `RendererLoopOwner` was unresolved. After implementation, the three direct
  owner tests pass, and the combined `renderer_` filter passes 35/35 across
  loop, frame, input wake/routing, acknowledgements, runtime inbox/event, and
  typed event routing.
- The closed runtime-surface validator and all deletion self-tests pass after
  moving the five lower-owner production anchors. Current measured files are
  `app.rs` 7,712 lines, `renderer_loop.rs` 438 lines, `renderer_frame.rs` 525
  lines, `presentation.rs` 75 lines, and `types.rs` 8,936 lines.
- Ordinary and test compiler gates, both validator families and self-tests,
  formatter, manifest digest, and diff checks pass. The full serial TUI suite
  passes 1,145/1,145 and the root PTY contract passes 6/6. CodeRabbit reviewed
  all 13 staged files with no Critical or Important finding; its one Minor was
  this status transition from Proposed to Implemented.
- The semantic commit rebased onto current local `main` without change, then
  fast-forwarded local `main`. Root verification repeated the 35/35 affected
  renderer tests, both validator families and self-tests, formatter, diff
  check, the full serial TUI 1,145/1,145, and PTY 6/6.
- After root verification, only `.worktrees/tui-renderer-loop-owner` and
  `codex/tui-renderer-loop-owner` were removed and pruned. The root worktree is
  clean and all unrelated worktrees remain registered.

## Out Of Scope

- Moving initial terminal title/draw, terminal cleanup, receiver close,
  mention-worker shutdown, agent cancellation/join, or exit session-id
  selection.
- Changing any input shortcut, event payload, reducer, attachment fence,
  approval/interaction policy, frame cadence, batch limit, retry, timeout, or
  disconnect rule.
- Adding buffering, cloning, threads, retries, catches, persistent state,
  traits, or a second state/event reducer.
- Cold legacy registry reconciliation, provider, runtime protocol,
  persistence, server, JSONL, ACP, shell, or release work.
