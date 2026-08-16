# TUI Renderer Input Wake Ownership

Status: Implemented

## Context

At audited base `efba5bf15`, `app.rs` is 7,917 lines. Terminal-session
startup, qwertty input production, renderer frame policy, and runtime-event
routing already have focused owners. `run_tui_inner` still owns the input wake
and terminal suspend protocol directly inside the frame loop:

1. poll the ordinary-input, focus, and control receivers with control-first,
   focus-second biased admission;
2. cap ordinary input at 64 events while allowing focus changes past that cap;
3. filter pointer-motion events before event coalescing;
4. acknowledge the first `Suspend` before blocking the renderer;
5. while suspended, acknowledge repeated `Suspend` messages and ignore them;
6. resume terminal rendering only after `InputControl::Resumed`;
7. translate an abandoned acknowledgement, suspended control disconnect, or
   ordinary wake disconnect into the existing exact `io::Error`; and
8. treat timeout or an unsolicited `Resumed` as an empty input batch.

`input_wake.rs` already owns the lower-level biased-select primitive, but it
does not retain receiver ownership or implement the suspend handshake. Keeping
that lifecycle loop in `app.rs` makes frame resumption and input admission
depend on application-level locals.

## Decision

Add `renderer_input_wake.rs` with one private `RendererInputWakeOwner`. It
consumes the three cloned receivers exposed by `TerminalInputReceivers`,
retains the ordinary event limit, and returns one filtered `Vec<Event>` per
renderer poll. Its `receive` method owns the current suspend acknowledgement
and blocking resume protocol and invokes the existing renderer-frame resume
callback only for an admitted `Resumed` control.

`input_wake.rs` remains the stateless priority and bounded-batch primitive.
`input_runtime.rs` remains the only qwertty thread, signal, mode, terminal
lease, producer, leave, and join owner. `RendererFrameOwner` remains the only
resume clear/title invalidation/dirty owner.

This is an ownership extraction. It adds no receiver, sender, input thread,
control message, acknowledgement, terminal transition, retry loop, or frame
clock.

## Frozen Wake Semantics

### Admission And Backpressure

1. Control messages remain higher priority than focus events, and focus events
   remain higher priority than ordinary input.
2. The ordinary input limit remains 64. Focus events bypass that limit, but
   only 64 ordinary events may accompany one focus wake.
3. Ordinary overflow remains queued on the existing bounded receiver.
4. Pointer motion with no button remains filtered before coalescing; all other
   admitted events retain source order.
5. Timeout and an unsolicited `InputControl::Resumed` return an empty batch.

### Suspend And Resume

1. The first `Suspend` acknowledgement is sent before waiting for another
   control message. A dropped acknowledgement receiver returns
   `BrokenPipe("terminal input runtime dropped suspend acknowledgement")`.
2. While suspended, the renderer does not consume ordinary or focus input.
3. A repeated `Suspend` is acknowledged best-effort and waiting continues.
   Failure of that repeated acknowledgement remains ignored.
4. The first `Resumed` invokes the supplied resume callback exactly once, then
   returns an empty batch.
5. A control-channel disconnect while suspended returns
   `UnexpectedEof("terminal input runtime disconnected while suspended")`.
6. A disconnected wake outside suspension returns
   `UnexpectedEof("terminal input runtime disconnected")`.
7. Resume callback errors remain the renderer-loop error without translation.

## Ownership And Compatibility

- `renderer_input_wake.rs` owns the cloned input receivers, ordinary batch
  limit, event filtering, and renderer-side suspend/resume handshake.
- `input_wake.rs` keeps biased selection and bounded draining semantics.
- `terminal_session.rs` keeps cloning the three receivers from the one
  `InputRuntime`; it transfers them without changing capacity or identity.
- `renderer_frame.rs` keeps terminal resume mechanics and all frame policy.
- `app.rs` keeps insert-escape expiry, interaction acknowledgements, input
  coalescing and semantic routing, runtime-event iteration, drawing, cleanup,
  and host shutdown.
- No `TuiEvent`, `UserAction`, `InputControl`, runtime surface, CLI/slash
  syntax, server/JSONL, app-server, ACP, history, schema, persistence,
  environment lookup, terminal escape sequence, or public Rust API changes.
- No channel capacity, priority, event limit, frame timing, error text,
  cancellation, shutdown order, or visible TUI behavior changes.

## Validator Contract

Add one closed `renderer_input_wake` TUI entrypoint with path-specific app and
owner anchors. Negative self-tests must prove that imports, type definitions,
unit fixtures, lower-level `receive_prioritized_input_or_control` calls, or
unrelated `renderer_frame.resume` calls cannot mask deletion of receiver
ownership, event filtering, first suspend acknowledgement, repeated suspend
handling, resumed callback, or either exact disconnect path.

## Test Strategy

1. Register a test-only module that imports the absent owner and require RED
   before production implementation.
2. Directly prove control priority, first and repeated acknowledgement order,
   no queued-key consumption while suspended, and exactly one resume callback.
3. Directly prove a dropped first acknowledgement and a suspended control
   disconnect preserve their exact error kinds and text.
4. Directly prove timeout/unsolicited resume return empty, pointer motion is
   filtered, ordinary input stays bounded, and ordinary disconnect preserves
   its exact error.
5. Keep existing input-runtime signal, lower-level input-wake priority/focus
   cap, presentation resume, frame, full serial TUI, and PTY tests as
   downstream evidence.
6. Run compiler checks, runtime and Windows validators plus self-tests,
   formatter, diff check, full TUI, and PTY gates before and after rebase and
   again on integrated local `main`.

## Acceptance Criteria

1. `run_tui_inner` no longer stores or directly polls input receivers, sends
   suspend acknowledgements, blocks for resume, or translates wake disconnects.
2. One private owner implements the exact existing priority, limit, filtering,
   acknowledgement, resume, and error semantics.
3. Direct owner tests are RED before implementation and GREEN afterward.
4. Existing input/runtime/frame/presentation ownership and every public,
   protocol, persistence, full TUI, and PTY behavior remain compatible.
5. Independent review has no unresolved Critical or Important finding.
6. After local-main integration and root verification, remove only this slice
   worktree and merged topic branch immediately.

## Implementation Evidence

- The direct owner suite first failed RED with `E0432` because
  `RendererInputWakeOwner` did not exist. The implemented owner suite now
  passes 7/7, covering repeated Suspend acknowledgements and one resume,
  dropped first acknowledgement, suspended disconnect, resume error
  propagation, timeout/unsolicited resume, focus priority, filtering and
  ordinary cap, and ordinary disconnect.
- `app.rs` now constructs one `RendererInputWakeOwner` from the existing
  `TerminalInputReceivers` and delegates the wake/suspend protocol with the
  existing `renderer_frame.resume` callback. `input_wake.rs`,
  `input_runtime.rs`, and `renderer_frame.rs` retain their prior ownership.
- Focused downstream evidence passes: lower-level suspend/focus priority (2),
  bounded focus backpressure (1), input runtime (13), presentation resume (1),
  and renderer frame (6). Ordinary `cargo check -p orca-tui --locked` passes.
- The runtime-surface validator has a closed `renderer_input_wake` entrypoint
  with path-specific app/owner anchors. Negative self-tests cover app
  delegation, receiver transfer, priority selection, filtering, first and
  repeated acknowledgements, resume callback, and both disconnect routes.
  The validator and self-tests pass. The reviewed manifest and digest share
  manifest SHA-256
  `ce43e667a1ed04e0801d36a15914f0959d5423560e628ced242c83cabb1e55f0`.
- Post-extraction source sizes are `app.rs` 7,880 lines,
  `renderer_input_wake.rs` 298 lines, `terminal_session.rs` 220 lines,
  `input_wake.rs` 124 lines, and `renderer_frame.rs` 525 lines. Full serial
  TUI passes 1,125/1,125 and PTY passes 6/6. CodeRabbit's staged review covered
  all 12 slice files and raised 0 issues. Rebase was a clean no-op on the latest
  local `main`; affected, full serial TUI, and PTY gates passed afterward and
  again on integrated root. The merged slice worktree and topic branch were
  removed immediately after root verification.

## Out Of Scope

- Moving semantic scroll/focus/paste/resize/mouse/key routing.
- Moving insert-escape expiry, input coalescing, interaction acknowledgement,
  runtime-event handling, frame scheduling, drawing, or terminal cleanup.
- Rewriting qwertty input production, signals, capability probing, terminal
  modes, channel backpressure, or presentation resume mechanics.
- Cold legacy registry reconciliation, pending-store retirement, runtime
  protocol, or persistence work.
