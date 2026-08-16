# TUI Terminal Session Startup Ownership

Status: Implemented

## Context

At audited base `b93686996`, `app.rs` is 7,952 lines. The qwertty input
thread, terminal-presentation lifecycle, renderer runtime-event coordination,
and renderer frame policy already have focused owners. `run_tui_inner` still
assembles the terminal session itself across two distant phases:

1. start `InputRuntime` and acquire the process terminal lease;
2. resolve the theme from the probed terminal profile;
3. clone ordinary-input, focus, and suspend/resume control receivers;
4. derive the terminal-presentation profile and construct its state machine;
5. construct the capability-adapting crossterm backend;
6. start the hosted agent/controller while the input runtime remains pending;
7. explicitly finish pending input if agent startup fails;
8. only after agent startup, construct and clear the ratatui terminal; and
9. hand the terminal, presentation, and input runtime to the existing cleanup
   scope.

This ordering is deliberate. Input probing and mode ownership must precede
ratatui construction, agent startup failure must restore terminal input, and
ratatui must drop before qwertty leaves the terminal on normal cleanup. Keeping
the pending and activated states as unrelated locals makes those invariants
implicit in one large application function.

## Decision

Add `terminal_session.rs` with a private two-state startup owner:

- `PendingTerminalSession` owns the started `InputRuntime`, resolved `Theme`,
  cloned input receivers, `TerminalPresentation`, and unopened terminal
  backend.
- `ActivatedTerminalSession` owns the same facts after ratatui construction
  and the startup clear. It can be consumed into the existing application
  locals and presentation-cleanup resource tuple.

`app.rs` creates the pending owner before any runtime or renderer state, starts
the hosted agent/controller exactly where it does today, delegates the failure
finish path to the pending owner, and activates only after agent startup and
composer/renderer preparation.

This is an ownership extraction. It does not create another input thread,
terminal lease, event/control channel, presentation state machine, terminal
backend, cleanup guard, frame scheduler, or host runtime.

## Frozen Startup Semantics

### Pending Session

1. `InputRuntime::start` remains the first fallible terminal-session action.
2. Theme resolution uses the exact probed `TerminalProfile` returned by that
   runtime.
3. Ordinary input, focus input, and control receivers remain clones of the
   runtime's existing bounded receivers. Capacities and priority semantics do
   not change.
4. `TerminalPresentationProfile` is still derived from the same qwertty
   environment identity, and terminal notifications still control both focus
   events and presentation notification output.
5. The backend remains `CapabilityBackend<CrosstermBackend<RetryWriter<Stdout>>>`
   using the resolved terminal color level.
6. No ratatui `Terminal` exists in the pending state.

### Agent Startup Boundary

1. Hosted agent/controller construction stays in `app.rs`; the terminal
   session does not gain runtime-host, action, event, or shutdown authority.
2. Agent startup still happens after input ownership is established and before
   ratatui terminal construction.
3. If agent startup fails, the pending input runtime is explicitly finished.
   If finish succeeds, the original agent error is returned. If finish fails,
   the finish error remains the returned error, matching the existing `?`
   precedence.
4. No terminal activation or initial title/draw occurs on that failure path.

### Activation And Presentation

1. Activation consumes the pending owner, constructs ratatui from the existing
   backend, and clears it once before returning the activated state.
2. `Terminal::new` and startup-clear failures still unwind the pending input
   runtime through its idempotent `Drop` path.
3. Initial title output followed by the first draw remains in
   `initialize_terminal_presentation`; activation does not write title output
   or render `AppState`.
4. `presentation.rs` remains the owner of resume clear/invalidation, initial
   title-before-draw ordering, cleanup error precedence, reset-title output,
   ratatui drop, and input finish ordering.
5. Renderer frame/runtime owners receive the same `Theme`, receivers,
   terminal, presentation, and input runtime values as before.

## Ownership And Compatibility

- `terminal_session.rs` owns terminal-session assembly and the pending-to-active
  startup transition only.
- `input_runtime.rs` remains the sole terminal lease, qwertty session, mode,
  capability-probe, signal, input-thread, channel producer, leave, and join
  owner.
- `presentation.rs` remains the sole terminal presentation initialization,
  resume, finish, and cleanup-order primitive owner.
- `app.rs` keeps hosted runtime construction, state/composer construction, the
  frame loop, renderer coordination, shutdown, and `TuiExit` construction.
- No `TuiEvent`, `UserAction`, runtime surface, CLI/slash syntax, server/JSONL,
  app-server, ACP, history, schema, persistence, environment lookup, terminal
  escape sequence, or public Rust API changes.
- No channel capacity, input priority, capability timeout, color adaptation,
  notification policy, frame cadence, terminal cleanup order, agent shutdown
  order, or error text changes.

## Validator Contract

Add one closed `terminal_session_startup` TUI entrypoint with path-specific
anchors for the app pending-owner construction, hosted-agent failure route,
activation call, and production owner. Negative self-tests must prove that an
import, type definition, test fixture, or unrelated `InputRuntime::start`,
`Terminal::new`, `Terminal::clear`, or `finish` call cannot mask deletion of
the production two-phase path or its error-precedence route.

## Test Strategy

1. Add direct module tests before the owner exists and require RED on the
   absent startup/activation API.
2. With a ratatui test backend, prove activation performs the startup clear and
   preserves pending resources into the activated state.
3. With injected finish outcomes, prove successful input finish returns the
   original agent error while failed finish returns the finish error.
4. Keep the existing input-runtime startup/failure/Drop/lease tests,
   title-before-initial-draw test, cleanup-after-body-error test,
   reset/drop/finish order test, full serial TUI suite, and PTY contract as
   downstream evidence.
5. Run compiler checks, runtime and Windows validators plus negative
   self-tests, formatter, diff check, full TUI, and PTY gates before and after
   rebase and again on integrated local `main`.

## Acceptance Criteria

1. `app.rs` no longer directly starts `InputRuntime`, derives terminal-session
   receivers/presentation/backend, constructs ratatui, or performs the
   agent-startup input-finish path.
2. Pending and activated terminal-session states make it impossible to
   construct ratatui before the explicit activation boundary.
3. Direct owner tests are RED before implementation and GREEN afterward,
   including startup clear/resource preservation and agent/finish error
   precedence.
4. The extraction preserves terminal/input ownership, first-frame ordering,
   cleanup ordering, all compatibility surfaces, and every focused/full gate.
5. Independent review has no unresolved Critical or Important finding.
6. After local-main integration and root verification, remove only this slice
   worktree and merged topic branch immediately.

## Implementation Evidence

- The direct owner suite first failed RED with `E0432` unresolved imports for
  `activate_terminal_session_with` and `finish_startup_failure_with`, proving
  the production owner API was absent before implementation.
- `terminal_session` now passes 2/2 direct tests covering create-before-clear,
  owned-resource preservation, and original-agent versus input-finish error
  precedence.
- Ordinary `cargo check -p orca-tui --locked` passes. Focused input-runtime,
  terminal-presentation, renderer-frame, suspend/resume, title-before-draw,
  presentation-exit, and runtime-surface contract tests preserve the existing
  downstream lifecycle behavior.
- The runtime-surface validator and its deletion self-tests pass with a closed
  `terminal_session_startup` entrypoint and path-specific production anchors.
  The reviewed manifest and digest share SHA-256
  `6946c89c2be04f9d0a06fad56a201dd8ae218f73269bad9f122581b855a218f3`.
- After extraction, `app.rs` is 7,917 lines and `terminal_session.rs` is 207
  lines. The full serial TUI suite passes 1,118/1,118 and the root PTY
  contract passes 6/6. CodeRabbit reviewed all 11 intended files with zero
  findings. A no-op rebase onto clean local `main` preserves owner 2/2, full
  TUI 1,118/1,118, PTY 6/6, validator, formatter, diff, and digest results.
- Local `main` fast-forwarded cleanly and independently passed full TUI
  1,118/1,118, PTY 6/6, both validators and their negative self-tests,
  formatter, diff, and digest checks. The slice worktree and merged topic
  branch were then removed immediately; unrelated worktrees remain intact.

## Out Of Scope

- Rewriting qwertty input, capability probing, signal handling, terminal mode
  restoration, panic restoration, or channel backpressure.
- Moving hosted-agent construction, initial AppState/composer construction,
  frame scheduling, event routing, rendering, or supervised shutdown.
- Changing presentation encoding, initial draw contents, terminal cleanup
  error policy, backend retry behavior, or public configuration.
- Cold legacy registry reconciliation, pending-store retirement, runtime
  protocol, or persistence work.
