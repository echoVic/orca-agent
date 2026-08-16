# TUI Terminal Lifecycle Ownership

Status: Implemented

## Context And Root-Cause Evidence

At audited base `cb7d15d6c`, `app.rs` is 7,712 lines and
`terminal_session.rs` is 220 lines. `PendingTerminalSession` already owns input
startup, capability/theme resolution, backend creation, hosted-agent startup
failure cleanup, and the pending-to-activated transition. After activation,
`run_tui_inner` immediately dismantles `ActivatedTerminalSession` into a theme,
three input receivers, and a `(terminal, presentation, input_runtime)` tuple.
It then constructs `RendererInputWakeOwner` and directly composes initial
presentation, the renderer loop, terminal cleanup, and input-runtime finish.

This is an architecture boundary defect: the type that proves terminal
activation no longer owns the active lifetime or cleanup. It also exposes one
local cleanup defect in `presentation::finish_terminal_presentation`:

```rust
reset_title(&mut terminal)?;
drop_terminal(terminal);
finish_input()
```

If the generic primitive receives a failing reset callback, that error returns
before either explicit callback. Production intentionally wraps
`write_reset_title` in `let _ = ...; Ok(())`, so title-write failure remains
best-effort and does not enter this error branch. The direct primitive contract
is still incomplete: a future failing caller could skip terminal retirement
and input finish, contradicting the resource-ownership rule that cleanup must
continue after a preceding cleanup step fails.

The existing focused tests prove only successful reset/drop/finish order and
cleanup after a renderer-body error. They do not cover reset-title failure.
Baseline `terminal_session` passes 2/2 and `presentation_exit` passes 2/2.

The same early-return shape exists one layer higher. The result of terminal
presentation cleanup is immediately unwrapped with `?` before
`RendererRuntimeEventOwner::shutdown`, `RendererRuntimeInboxOwner::shutdown`,
and `TuiAgentRuntime::shutdown`. A renderer or terminal error therefore skips
all three explicit calls. `MentionSearchManager::Drop` and
`TuiAgentRuntime::Drop` eventually invoke their shutdown paths, but the inbox
close order is implicit and all shutdown failures are discarded. This is the
same root cause: cleanup is encoded as success-only control flow rather than a
total lifecycle boundary.

## TUI Value And Independent Slice

The user-facing value is reliable terminal restoration on every orderly exit
and renderer error: ratatui is retired before qwertty input cleanup, and the
input thread is explicitly stopped and joined instead of relying on an
implicit destructor path. The production title reset remains best-effort, as
before, and cannot prevent later cleanup. Renderer errors also close
the bounded runtime inbox and explicitly cancel/join the hosted controller and
runtime host. The architectural value is one active-session owner plus one
post-terminal coordinator that make all cleanup phases unavoidable.

This slice is independently reviewable and reversible. It changes no TUI
command, rendering policy, runtime event, session identity, protocol, or
persistence state. It does not move hosted-agent shutdown into the terminal
owner.

## Decision

Extend the existing private two-state terminal owner rather than introducing a
parallel session abstraction:

1. `PendingTerminalSession::activate` continues to construct and clear
   ratatui, but returns an `ActivatedTerminalSession<Terminal, Input>` that
   retains the terminal, presentation, input runtime, resolved theme, and input
   receivers as owned fields.
2. A consuming `ActivatedTerminalSession::run` constructs the single existing
   `RendererInputWakeOwner`, admits one renderer-body closure, and always runs
   presentation/input cleanup after the body returns.
3. A generic private `run_with` form supplies injected reset, terminal-drop,
   and input-finish operations for direct behavior tests. Production `run`
   binds those operations to the existing best-effort title reset through the
   live ratatui backend, `drop`, and `InputRuntime::finish`.
4. `finish_terminal_presentation` becomes a total cleanup sequence. It records
   reset-title failure, always drops ratatui, always calls input finish, then
   returns the reset error when present or the input-finish result otherwise.
5. `app.rs` matches terminal activation into the same result later consumed by
   the post-terminal coordinator. Success invokes the owner method; activation
   failure preserves its exact error without skipping explicit runtime
   shutdown. Initial title then draw and `RendererLoopOwner` construction
   remain in the body closure; hosted runtime, renderer-runtime/inbox,
   exit-session selection, and shutdown remain in app.
6. Add `tui_run_lifecycle.rs` with a small `finish_tui_run` coordinator. It
   receives the activation/terminal/renderer result and the existing renderer, inbox, and
   agent shutdown operations; invokes all three in the frozen order; then
   returns the renderer result when it failed or the agent-shutdown result when
   rendering succeeded.

No new thread, channel, cancellation token, terminal lease, renderer loop,
presentation state machine, or state source is added.

## Frozen Behavior

### Normal Startup, Run, And Exit

1. `InputRuntime::start`, terminal-profile probing, theme resolution, receiver
   cloning, presentation creation, and backend creation remain unchanged.
2. Hosted-agent startup still occurs before ratatui activation. Its failure
   still explicitly finishes pending input and preserves existing error
   precedence.
3. Activation still constructs ratatui and clears it exactly once.
4. The activated owner constructs exactly one `RendererInputWakeOwner` with
   the current maximum of 64 ordinary input events per batch.
5. Initial title output precedes the first draw. The foreground renderer loop
   then preserves its existing ordering and returns the exact exit code.
6. Orderly cleanup is attempt title reset, drop ratatui, finish qwertty/input thread,
   stop mention search, close the runtime inbox, then cancel/join the hosted
   agent and runtime host. The latter three operations now run after both a
   successful terminal body and a failed terminal body/cleanup.

### Failure And Error Precedence

1. Terminal construction or startup-clear failure unwinds the pending input
   runtime through its idempotent `Drop` implementation, as before. Its exact
   error is then passed to `finish_tui_run`, so renderer shutdown, inbox close,
   and agent shutdown are still explicit.
2. Initial-title, initial-draw, renderer-loop, resume, event-routing, terminal
   output, and draw errors all enter the same active-session cleanup path.
3. In the generic owner contract, a renderer-body error remains the returned
   error even when an injected reset callback or input finish also fails.
   Cleanup still runs completely.
4. If the body succeeds but an injected reset callback fails, ratatui is still
   dropped and input finish is still called. The reset error is returned.
   Production preserves its prior best-effort title-write normalization.
5. If the injected reset callback succeeds and input finish fails, the
   input-finish error is returned. If both fail, reset remains the returned
   error.
6. Renderer-runtime shutdown, inbox close, and agent shutdown always run after
   activation has failed or active-terminal cleanup has completed, including
   body, reset, or input-finish failure. A prior activation/body/terminal error
   remains the returned error; otherwise agent shutdown error is returned.
   Infallible renderer shutdown and inbox close do not add fabricated results.
7. Cleanup does not retry writes or joins, translate errors, fabricate a TUI
   event, or aggregate multiple errors. Existing final `?` propagation from
   `run_tui_inner` remains after total cleanup completes.

### Cancellation, Rejection, Timeout, Retry, Disconnect, And Restart

1. Keyboard/runtime cancellation, approval rejection, user-input settlement,
   timeout, retry, compaction, and operation recovery remain below the renderer
   event owners and are unaffected.
2. Input suspend/resume and disconnect translation remain in
   `RendererInputWakeOwner`; moving its construction changes no channel or
   priority behavior.
3. Runtime-inbox disconnect remains an empty non-blocking iterator. Runtime
   event and acknowledgement bounds are unchanged.
4. Restart constructs a fresh pending/activated terminal session and reads the
   same runtime-owned session snapshot. No terminal state is persisted.

## Ownership And Compatibility

- `terminal_session.rs` owns pending and active physical terminal lifetime,
  active input-receiver adaptation, and cleanup completion.
- `input_runtime.rs` remains the sole qwertty thread, process terminal lease,
  capability probe, input production, stop signal, join, and emergency restore
  owner.
- `presentation.rs` remains the generic owner of title/draw/resume/finish order
  and body-versus-cleanup error precedence.
- `renderer_loop.rs` remains the sole foreground iteration owner.
- `tui_run_lifecycle.rs` owns only the post-terminal shutdown order and
  renderer-result-versus-agent-shutdown error precedence.
- `app.rs` retains application state, hosted runtime, action/event channels,
  renderer collaborator construction and `TuiExit` construction.
- No CLI, TUI action/event, runtime surface, server/JSONL, app-server, ACP,
  public Rust API, history, persistence, environment, or schema change.

## Validator Contract

Add one closed `terminal_session_lifecycle` TUI entrypoint. Its path-specific
anchors must prove app activates and runs the owner, the activated owner creates
the input-wake owner and delegates body/cleanup, and presentation cleanup
continues through reset failure. Migrate the renderer-input-wake production
construction anchor from `app.rs` to `terminal_session.rs` while retaining its
renderer-loop and focused-owner anchors.

The same closed entrypoint must anchor `finish_tui_run` in app and its focused
module. It must prove renderer shutdown precedes inbox close, agent shutdown is
always attempted, and a renderer result has precedence over a simultaneous
agent-shutdown error.

Negative self-tests must prove imports, the prior startup entrypoint, direct
helper tests, unrelated `run`/`finish` calls, or lower-owner constructors cannot
mask deletion of app delegation, owner construction/order, reset-error
totalization, or renderer-input-wake migration. No mutation or harmless-method
baseline may be broadened.

## Test Strategy

1. Add a direct presentation test first. RED must show reset failure records
   only `reset`, not `reset`, `drop`, `finish`.
2. Totalize `finish_terminal_presentation`; require the exact reset error while
   both later cleanup callbacks run.
3. Add a direct activated-owner test first using a generic test terminal/input,
   real bounded input channels, and the wished-for `run_with` API. RED must fail
   because that method/state shape does not exist.
4. Prove a queued input is visible through the owner-created input wake, a body
   error still runs reset/drop/finish in order, and the body error wins.
5. Migrate app to the owner, remove obsolete direct cleanup and input-wake
   construction, and keep title-before-draw plus renderer-loop tests green.
6. Add a direct post-terminal coordinator test first. RED must fail because
   `finish_tui_run` is absent. Prove all three shutdown callbacks run after a
   renderer error and that the exact renderer error wins over an agent error.
7. Run focused terminal-session, presentation, TUI-run-lifecycle, input-wake, renderer-loop,
   input-runtime, and lifecycle tests; compiler gates; both validators and
   self-tests; formatter; digest; diff check; full serial TUI; and PTY before
   and after rebase and on integrated local `main`.

## Acceptance Criteria

1. `app.rs` no longer dismantles `ActivatedTerminalSession`, constructs
   `RendererInputWakeOwner`, or composes terminal/input cleanup.
2. One consuming activated-session method owns the live terminal resources,
   input-wake adapter, renderer-body scope, and mandatory cleanup.
3. An injected reset failure cannot skip explicit terminal drop or input
   finish, production retains best-effort title-reset behavior, and all generic
   error-precedence rules have direct behavior tests.
4. Initial presentation, renderer-loop, post-terminal shutdown, and exit
   session-id behavior remain unchanged.
5. Renderer or terminal failure cannot skip explicit mention shutdown, inbox
   close, or agent cancellation/join; direct tests prove order and error
   precedence.
6. Closed validator anchors and deletion self-tests cover the production path
   without a new compatibility layer or second owner.
7. Focused/full/PTY gates pass and independent review has no unresolved
   Critical or Important finding.
8. After local-main integration and root verification, immediately remove only
   this slice's worktree and topic branch.

## Implementation Evidence

- Presentation RED: the reset-failure regression observed only `reset`; GREEN
  observes exact `reset`, `drop`, `finish` order and retains the reset error.
- Activated-session RED: the direct owner test failed to compile because the
  generic retained fields and `run_with` did not exist; GREEN consumes queued
  input, runs total cleanup after a body error, and preserves that body error.
- Post-terminal RED: the coordinator test failed on unresolved
  `finish_tui_run`; GREEN proves renderer, inbox, and agent shutdown order plus
  renderer-error and agent-error precedence.
- Final review exposed activation's remaining early `?`. Tightening the app
  lifecycle anchor first failed on the old path; matching activation into the
  coordinator result made that contract GREEN and preserves the activation
  error while explicitly shutting down all three runtime owners.
- Focused filters pass: activated session 1/1, presentation 25/25, terminal
  session 3/3, renderer input-wake 7/7, renderer loop 3/3, input runtime 13/13,
  and TUI-run lifecycle 2/2.
- `cargo check -p orca-tui --locked` and
  `cargo check -p orca-tui --tests --locked` pass with only existing warnings.
- `cargo test -p orca-tui --lib --locked -- --test-threads=1` passes 1,149/1,149.
- `cargo test --test tui_pty_contract --locked -- --test-threads=1` passes 6/6.
- Rebase onto fetched local `main` was a no-op because the topic was current;
  the full 1,149-test TUI suite, 6-test PTY contract, and both validator suites
  passed again after that rebase check.
- Runtime-surface and Windows-boundary validators and both self-test suites
  pass. `cargo fmt --all -- --check` and `git diff --check` pass.
- Two scoped CodeRabbit reviews of the complete staged slice raised 0 issues.
  A broader committed-history review then identified the activation early
  return above; the slice was reopened to fix and reverify that valid Major.
- The post-fix incremental CodeRabbit retry was rate-limited after the three
  available reviews, so no post-fix CodeRabbit result is claimed. Fresh
  compiler, focused, full TUI, PTY, structural validator, negative self-test,
  formatter, and diff gates cover the fix.
- The manifest SHA-256 recorded in the reviewed digest matches the final
  manifest bytes.

## Migration And Rollback

Migration is atomic in one semantic commit: totalize the cleanup primitive,
retain active resources in `ActivatedTerminalSession`, move app composition to
its consuming method, totalize post-terminal runtime cleanup, migrate
tests/validators/docs, and delete the old app path. No dual production route is
permitted.

Rollback restores `into_parts`, app-owned input-wake/cleanup composition, the
former early-return helper, and success-only explicit runtime shutdown. There
is no data migration or cleanup.

## Out Of Scope

- Moving hosted-agent construction, renderer-runtime/inbox ownership,
  `TuiExit`, or resume-hint printing.
- Changing renderer iteration, input coalescing, runtime-event reduction,
  notification bytes, frame cadence, or title contents.
- Adding retries, timeouts, aggregate error types, logging, new threads,
  channels, guards, or persistent terminal state.
- Cold legacy registry reconciliation, provider/runtime protocol, server,
  JSONL, ACP, persistence, DeepSeek API, or release work.
