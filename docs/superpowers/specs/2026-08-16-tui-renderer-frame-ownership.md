# TUI Renderer Frame Ownership

Status: Implemented

## Context

At audited base `889472193`, `app.rs` is 8,236 lines. The hosted controller
and renderer runtime-event coordination already have focused owners, but
`run_tui_inner` still directly owns the renderer frame policy around the input
and runtime event loop:

1. the `FrameScheduler` instance and its initial drawn watermark;
2. edit-highlight result polling and dirty-frame admission;
3. animation demand, copy-notice expiry, state/title ticks, and drag scrolling;
4. the input poll timeout derived from dirty and animation deadlines;
5. resume-time terminal clearing, title invalidation, and dirty marking;
6. bounded input/runtime iteration scheduling and its exit/draw outcome;
7. staged clipboard delivery, pending title/notification output, terminal draw,
   and successful-draw acknowledgement.

These behaviors form one renderer-local timing and output boundary. Leaving
them as mutable locals in `run_tui_inner` makes their ordering depend on a wide
closure that also owns input routing, runtime-event reduction, terminal
lifecycle, and host shutdown.

## Decision

Add `renderer_frame.rs` with one private `RendererFrameOwner`. It owns the
existing `FrameScheduler` and exposes focused operations to prepare one loop
iteration, mark a renderer mutation dirty, resume rendering, run the existing
bounded event iteration, and present its resulting frame.

`frame_scheduler.rs` remains the pure timing and input/runtime fairness
primitive. `presentation.rs` keeps terminal initialization, cleanup, and the
generic clear/invalidate/dirty resume sequence. `terminal_presentation.rs`
keeps title and desktop-notification encoding and queue semantics.

This is an ownership extraction. It does not add another frame clock, event
queue, renderer state cache, background worker, retry loop, or presentation
protocol.

## Frozen Frame Semantics

### Iteration Preparation

1. Pending Vim insert-escape expiry remains before frame preparation.
2. Edit-highlight results are polled once per loop. Only an applied result
   marks the scheduler dirty; failed, stale, and disconnected results preserve
   their current redraw behavior.
3. Animation demand is computed before an expired copy notice is cleared. This
   preserves the final animation tick and dirty frame that removes the notice.
4. Animation remains active for running state, a copy notice, edge-drag
   scrolling, pending edit highlighting, or terminal-presentation animation.
5. When the animation deadline is due, the owner advances `AppState` first,
   then `TerminalPresentation`, then applies edge-drag scrolling, and finally
   records the animation with the scheduler.
6. The input wake timeout is calculated from the same current instant,
   animation flag, 16 ms frame interval, and 80 ms animation interval.

### Event Iteration And Resume

1. Input coalescing and input/runtime batch limits remain unchanged.
2. Input events are still handled before runtime events, and the first exit
   code still stops the iteration before later events or a draw decision.
3. Every admitted input or runtime event still marks the frame dirty.
4. A terminal resume still clears the terminal, invalidates the cached title,
   and marks the renderer dirty in that order. Suspend acknowledgement and the
   blocking control loop remain in `app.rs`.
5. Interaction-response acknowledgements remain owned by the renderer runtime
   reducer path; `app.rs` drains them and asks the frame owner to mark the
   resulting presentation mutation dirty.

### Presentation Completion

1. Renderer-runtime composer synchronization and exit-code handling remain
   before presentation completion. An exiting iteration still skips clipboard,
   pending title/notification output, and drawing.
2. A staged clipboard copy is taken and delivered before terminal
   presentation output. It is consumed exactly once.
3. Pending title and desktop-notification output is attempted on every
   non-exiting iteration. Its existing best-effort error behavior is unchanged.
4. A terminal draw occurs only when the iteration produced `draw_at`.
5. The scheduler records `did_draw(draw_at)` only after the terminal draw
   succeeds. A draw error remains the loop error and cannot clear dirty state.

## Ownership And Compatibility

- `renderer_frame.rs` owns frame timing, animation coordination, resume redraw
  invalidation, bounded iteration scheduling, clipboard consumption, pending
  presentation output, terminal drawing, and successful-draw acknowledgement.
- `app.rs` keeps terminal/input/runtime construction, suspend control,
  interaction-ack draining, input routing, runtime-event delegation, terminal
  cleanup, renderer-runtime shutdown, and agent-runtime shutdown.
- `input_event_actions.rs` keeps mouse hit testing, selection, and clipboard
  staging. `edit_highlight.rs` keeps worker/result validation and cache
  mutation. `ui.rs` remains the renderer.
- No `TuiEvent`, `UserAction`, runtime surface, CLI/slash syntax, server/JSONL,
  app-server, ACP, history, schema, persistence, or public Rust API changes.
- No channel capacity, input/runtime batch limit, frame/animation interval,
  timeout, output encoding, notification capacity, clipboard size bound,
  cancellation, worker, terminal cleanup, or host shutdown order changes.

## Validator Contract

Add a closed `renderer_frame` TUI entrypoint with path-specific anchors for the
app caller and production owner. Attribute staged clipboard consumption to the
new owner instead of the broad app loop. Negative self-tests must prove that
owner imports, type construction, tests, or unrelated scheduler calls cannot
mask deletion of iteration preparation, clipboard consumption, pending
presentation output, terminal drawing, or successful-draw acknowledgement.

## Test Strategy

1. Add a direct owner test through the initially absent module/API. Start from
   a drawn idle frame with an expiring copy notice, prepare at the animation
   deadline, and assert the notice is cleared only after it still causes a tick
   and dirty draw admission.
2. Move the existing edit-highlight frame tests to the owner and keep their
   ready, failed, stale, disconnected, refined-render, and one-shot redraw
   assertions intact.
3. Add a direct presentation-completion test with a ratatui test backend. Stage
   clipboard text, schedule a draw, and assert one-shot clipboard consumption,
   pending-output invocation, rendered output, and clean scheduler state after
   a successful draw.
4. Keep existing frame-scheduler fairness, terminal presentation, input wake,
   suspend/resume, mouse selection, edit-highlight, full TUI, and PTY tests as
   downstream evidence.
5. Run compiler checks, the full serial TUI library suite, root-package PTY
   contract, runtime and Windows validators plus self-tests, formatter, and
   diff checks.

## Acceptance Criteria

1. `renderer_frame.rs` is the only production owner of renderer frame timing,
   animation preparation, resume dirtying, iteration scheduling, clipboard
   consumption, pending presentation output, terminal drawing, and draw
   acknowledgement.
2. Direct owner tests are RED before the owner exists and GREEN after the
   extraction, including the expiring-copy final-redraw invariant and one-shot
   presentation completion.
3. The moved behavior is semantically identical and all focused, full TUI,
   PTY, validator, formatter, and diff gates pass after rebase and on integrated
   local `main`.
4. Independent review has no unresolved Critical or Important finding.
5. After local-main integration and root verification, remove only the slice
   worktree and merged topic branch immediately.

## Implementation Evidence

- The direct owner suite first failed with `E0432` because
  `RendererFrameOwner` did not exist. After implementation, all six owner tests
  pass, including the expiring-copy final redraw, one-shot clipboard/pending
  output/draw completion, ready/failed/stale/disconnected edit-highlight
  admission, and refined-render cache behavior.
- `app.rs` now constructs the frame owner after the initial draw and delegates
  preparation, resume, bounded iteration, and presentation completion. Input
  wake/suspend, interaction acknowledgement draining, event routing,
  renderer-runtime composer sync, terminal cleanup, and agent shutdown did not
  move.
- Focused frame scheduler (4), terminal presentation (10), input event (26),
  edit-highlight (40), suspend/focus (2), and presentation-resume (1) tests
  pass. The ordinary production `cargo check -p orca-tui --locked` also passes.
- The runtime-surface manifest now records the app caller and production owner,
  and attributes clipboard delivery to `renderer_frame.rs`. Negative self-tests
  independently delete the app caller, iteration preparation, clipboard take,
  pending presentation output, terminal draw, and post-success draw
  acknowledgement while preserving masking references; the validator and
  self-tests pass.
- Post-extraction source sizes are `app.rs` 7,952 lines and
  `renderer_frame.rs` 525 lines. No public, protocol, or persistence inventory
  changed.
- Final pre-review verification passes `cargo check -p orca-tui --tests
  --locked`, both runtime and Windows validators plus their negative
  self-tests, formatter, and diff checks. The full serial TUI suite passes
  1,116 tests in 42.29 seconds, and the root PTY contract passes all 6 tests in
  10.03 seconds.
- CodeRabbit reviewed all 11 slice files and reported zero findings. No
  Critical or Important review issue remains.
- The topic commit rebased onto unchanged local `main`, then passed the owner
  suite (6), full serial TUI suite (1,116), PTY contract (6), compiler,
  validators plus self-tests, formatter, diff, clean-tree, and digest checks.
  After fast-forward integration, root independently passed all 1,116 TUI
  tests in 270.70 seconds and all 6 PTY tests in 9.68 seconds, along with the
  same structural gates. The slice worktree and merged topic branch were then
  removed immediately; unrelated worktrees remain present.

## Out Of Scope

- Moving terminal or input-runtime construction/cleanup, suspend blocking,
  input routing, runtime-event reduction, renderer-runtime mention sync, or
  hosted runtime shutdown.
- Changing frame cadence, event fairness, UI layout, title/notification
  encoding, clipboard transport, edit-highlight computation, or rendering
  algorithms.
- Cold legacy registry reconciliation, pending-store retirement, or runtime
  protocol changes.
