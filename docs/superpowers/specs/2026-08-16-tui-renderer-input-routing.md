# TUI Renderer Input Routing Ownership

Status: Implemented

## Context

At audited base `9c9144599`, `app.rs` is 7,880 lines. Terminal-session
startup, input wake/suspend coordination, renderer frame policy, and runtime
event routing already have focused owners. `run_tui_inner` still owns the
semantic input-routing protocol directly inside the frame iteration callback:

1. coalesced scroll input flushes a pending Vim insert-escape prefix, cancels
   the pending Vim command, then scrolls at the event timestamp;
2. focus gained/lost updates presentation focus and short-circuits every other
   input path;
3. key input resolves a pending insert-escape sequence before shortcuts;
4. paste input flushes a pending insert-escape prefix before paste ownership,
   and a consumed paste cancels the pending Vim command;
5. resize invalidates selection and short-circuits key routing;
6. mouse input flushes pending insert-escape state before hit testing;
7. a handled mouse event cancels the pending Vim command;
8. a confirming mouse click synthesizes plain Enter and dispatches directly to
   the status-specific handler, without running global key preflight;
9. a real key runs global/search/selection preflight before status-specific
   handling; and
10. clear-terminal failures and explicit exit codes propagate unchanged to the
    renderer loop.

The low-level policies already live in `input_event_actions.rs`,
`insert_escape.rs`, `key_event_actions.rs`, and `status_key_actions.rs`.
Keeping their ordering and short-circuit protocol in `app.rs` leaves the
renderer entrypoint responsible for one more large semantic state machine.

## Decision

Add `renderer_input_router.rs` with one private `RendererInputRouter`. A router
borrows the existing state, configuration, action sender, preloaded transcript,
composer, Vim state, theme, presentation, and initial prompt for one input
dispatch. Its `route` method accepts one existing `BatchedInputEvent`, the
event timestamp, and the existing clear-terminal callback, then returns the
same `io::Result<Option<i32>>` expected by `RendererFrameOwner`.

The router owns sequencing and short-circuit decisions only. It delegates to
the existing low-level helpers without changing their implementations or
public visibility. It owns no channel, thread, terminal resource, frame clock,
runtime event, durable state, or new retry/cancellation policy.

## Frozen Routing Semantics

### Scroll, Focus, Paste, Resize, And Mouse

1. `BatchedInputEvent::ScrollLines` first flushes any pending insert-escape
   prefix into the composer, then cancels the pending Vim command, then calls
   `handle_scroll_lines` with the supplied timestamp.
2. `FocusGained` and `FocusLost` update only `TerminalPresentation` focus and
   return without insert-escape, paste, resize, mouse, or key handling.
3. Every event that reaches semantic routing resolves pending insert-escape
   before paste/resize/mouse/key dispatch. A consumed insert-escape sequence
   returns immediately.
4. Paste and mouse events run `flush_pending_insert_escape_before_non_key`
   before their existing handlers. Paste continues to preserve search,
   composer, placeholder, history-navigation, and menu-refresh ownership.
5. A consumed paste cancels only pending Vim command state and returns.
6. Resize continues to invalidate selection and returns without key handling.
7. Mouse hit testing and scroll behavior remain in `handle_mouse_event`.
   `Handled` cancels pending Vim command state and returns.
8. `SyntheticEnter` cancels pending Vim command state, creates a plain Enter
   press, and calls `handle_status_key` directly. It does not run
   `handle_key_event_preflight`.
9. Non-key, non-focus, non-paste, non-resize, non-mouse events remain inert.

### Real Keys, Exit, And Errors

1. Real keys call `handle_key_event_preflight` before `handle_status_key`.
2. `KeyEventFlow::Continue` consumes the event, `Exit(code)` returns the same
   code, and `Unhandled` alone reaches status-specific routing.
3. `StatusKeyFlow::Exit(code)` returns the same code; `Continue` returns no
   exit.
4. The same `RunConfig`, shared config, action sender, preloaded transcript,
   textarea, Vim state, theme, and cloned initial prompt reach the existing
   handlers.
5. The supplied clear-terminal callback remains lazy. Any `io::Error` from
   preflight or status routing is returned unchanged.
6. `run_tui_inner` continues to call `Instant::now()` once for each routed
   input event and continues to mark/draw frames through the existing
   `RendererFrameOwner` rules.

## Ownership And Compatibility

- `renderer_input_router.rs` owns semantic input order, short circuits,
  synthetic Enter construction, and exit/error folding.
- `renderer_input_wake.rs` keeps receiver ownership, priority, filtering,
  suspend/resume, and disconnect translation.
- `input_event_actions.rs` keeps coalescing, focus mutation, paste, resize,
  scroll, mouse hit testing, and selection behavior.
- `insert_escape.rs` keeps pending insert-escape resolution and flush policy.
- `key_event_actions.rs` keeps global/search/selection preflight.
- `status_key_actions.rs` keeps setup, picker, approval, plan, recovery, idle,
  running, compacting, and Vim status behavior.
- `app.rs` keeps input coalescing, interaction acknowledgements, mixed
  input/runtime iteration, frame presentation, terminal cleanup, and host
  shutdown.
- No `TuiEvent`, `UserAction`, runtime surface, CLI/slash syntax, server/JSONL,
  app-server, ACP, history, schema, persistence, environment lookup, terminal
  escape sequence, or public Rust API changes.
- No input capacity, priority, event cap, coalescing width, shortcut, visible
  TUI behavior, cancellation, error text, exit code, or shutdown order changes.

## Validator Contract

Add one closed `renderer_input_routing` TUI entrypoint with path-specific app
and owner anchors. Negative self-tests must prove that imports, unit fixtures,
or the retained low-level helper definitions cannot mask deletion of app
delegation, scroll flush order, focus short circuit, insert-escape resolution,
paste/mouse preflush, resize handling, handled-mouse cancellation, synthetic
Enter direct status dispatch, real-key preflight, or exit/error folding.

## Test Strategy

1. Register a test-only module that imports the absent router and require RED
   before production implementation.
2. Directly prove focus is consumed before all semantic mutations.
3. Directly prove scroll and paste flush pending insert-escape input before
   their existing actions and cancel pending Vim command state as before.
4. Directly prove resize invalidates selection without key fallthrough.
5. Directly prove a selected plan option confirmed by mouse becomes the same
   status-owned implementation action as Enter.
6. Directly prove a real Esc uses preflight selection dismissal before running
   status behavior, and clear-terminal errors remain unchanged.
7. Keep existing input-event, insert-escape, key-preflight, status-key,
   renderer-frame, full serial TUI, and PTY tests as downstream evidence.
8. Run compiler checks, runtime and Windows validators plus self-tests,
   formatter, diff check, full TUI, and PTY gates before and after rebase and
   again on integrated local `main`.

## Acceptance Criteria

1. `run_tui_inner` no longer contains the semantic scroll/focus/paste/resize/
   mouse/key routing match or constructs synthetic Enter.
2. One private router preserves the exact frozen order, short circuits,
   collaborators, timestamp, exit codes, and error propagation.
3. Direct router tests are RED before implementation and GREEN afterward.
4. Existing low-level ownership and every public, protocol, persistence, full
   TUI, and PTY behavior remain compatible.
5. The runtime-surface contract has closed app/owner anchors plus deletion
   self-tests, without broadening mutation or harmless-method baselines.
6. Independent review has no unresolved Critical or Important finding.
7. After local-main integration and root verification, remove only this slice
   worktree and merged topic branch immediately.

## Evidence

- Base `9c9144599` passes the renderer input-wake owner suite 7/7, full serial
  TUI 1,125/1,125, PTY 6/6, runtime and Windows validators/self-tests,
  manifest digest, formatter, and diff check.
- The audited production routing block is `app.rs:326-449`; every delegated
  helper already has focused lower-level tests, but no owner-level test freezes
  their cross-module ordering.
- Current source sizes are `app.rs` 7,880 lines,
  `input_event_actions.rs` 1,460 lines, `insert_escape.rs` 84 lines,
  `key_event_actions.rs` 331 lines, and `status_key_actions.rs` 950 lines.
- The RED router suite failed only because `RendererInputRouter` was absent;
  after implementation its seven owner tests pass, including strengthened
  focus-before-insert-escape/Vim-command evidence and unchanged clear-error
  propagation.
- Existing input-event, insert-escape, key-preflight, status-key, and renderer
  frame focused suites pass with the app input arm reduced to one returned
  `RendererInputRouter::new(...).route(...)` delegation.
- The runtime contract validator and its deletion self-tests pass with a new
  `renderer_input_routing` inventory. The tests independently remove app
  delegation and every frozen production ordering segment while preserving
  masking imports, branches, or owner tests.
- Ordinary and test compiler gates, both contract validators and self-tests,
  formatter, digest equality, diff check, the full serial TUI suite
  (1,132/1,132), and the root PTY contract (6/6) pass in the topic worktree.
- CodeRabbit reviewed all eleven staged slice files and reported zero findings.
- After rebasing onto unchanged local `main@9c9144599`, the owner/affected
  suites, both validator families and self-tests, formatter, diff check, full
  serial TUI suite (1,132/1,132), and PTY contract (6/6) pass again.
- Local `main` fast-forwarded to the reviewed slice, then passed the router
  owner suite (7/7), both validator families and self-tests, the full serial
  TUI suite (1,132/1,132), and PTY contract (6/6). The owned worktree and
  merged topic branch were removed immediately afterward; unrelated worktrees
  remain registered and untouched.
- Implemented source sizes are `app.rs` 7,782 lines and
  `renderer_input_router.rs` 461 lines. All previously measured lower-level
  owner sizes remain unchanged.

## Out Of Scope

- Moving input wake, input coalescing, interaction acknowledgement, runtime
  event routing, frame scheduling/drawing, or terminal cleanup.
- Rewriting scroll, focus, paste, resize, mouse, selection, shortcut, status,
  setup, approval, plan, session picker, or Vim policies.
- Changing synthetic Enter into a queued terminal event or unifying it with
  real-key preflight.
- Cold legacy registry reconciliation, runtime protocol, persistence, provider,
  server, JSONL, ACP, shell, or release work.
