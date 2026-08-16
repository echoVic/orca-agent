# TUI Terminal Bootstrap Ownership

Status: Implemented

## Context And Root-Cause Evidence

At audited base `8a58f0ba5`, `app.rs` is 7,609 lines and
`terminal_session.rs` is 357 lines. The preceding active-terminal lifecycle
slice made `ActivatedTerminalSession` the unique owner of the ratatui terminal,
terminal presentation, qwertty input runtime, input receivers, renderer input
wake, and mandatory cleanup. It also made post-terminal renderer, inbox, and
hosted-agent shutdown total across activation, renderer, and cleanup failures.

One physical-terminal transition still remains outside that owner. After
activation, `run_tui_inner` directly invokes
`initialize_terminal_presentation`, writes the initial pending title, draws the
first frame, and only then constructs `RendererLoopOwner`. The active owner
receives this whole sequence as an opaque body closure, so its contract proves
only `body -> cleanup`, not `initial title -> first draw -> renderer -> cleanup`.
The current call site is correct, but the type boundary permits a future caller
to skip or reorder the first presentation without violating the owner API.

This is an architecture boundary defect, not a currently observed rendering
failure. The physical terminal owner should make the one required bootstrap
transition unavoidable. The existing presentation tests prove generic
title-before-draw ordering, and the active-owner test proves body failure still
cleans up, but no active-owner test proves initialization precedes the renderer
or that initialization failure skips the renderer while still running total
cleanup.

Cold reconciliation of pre-surface registry-only task rows is a separate
runtime/persistence boundary. Those rows lack typed operation, interaction,
and background-owner fences, and cannot safely become actionable merely by
being displayed during TUI bootstrap. This slice does not read, merge, or
project registry task rows.

## TUI Value And Independent Slice

The user-facing reliability value is deterministic first-screen startup and
terminal restoration: every activated TUI attempts the initial title and first
draw before accepting renderer input, and any failure in that bootstrap still
retires ratatui and joins input before runtime shutdown. The architecture value
is one active-terminal method whose type-level sequence covers the full
physical terminal lifetime rather than leaving its first transition in app.

This slice is independently reviewable and reversible. It changes no title
text, first-frame content, input timing, event ordering after the first frame,
exit code, session identity, runtime surface, protocol, or persistence state.

## Decision

Extend the existing active-terminal owner rather than introducing a second
bootstrap object:

1. `ActivatedTerminalSession::run` accepts the initial `AppStatus`, one generic
   renderer context, a one-shot first-draw callback, and the existing
   renderer-body callback. The owner holds the context for the run scope and
   lends `&mut Context` to the two callbacks sequentially; sibling closures do
   not capture overlapping mutable references.
2. Production `run` invokes `initialize_terminal_presentation` internally. Its
   title callback preserves the existing best-effort
   `TerminalPresentation::write_pending` behavior; its draw callback delegates
   to the caller with the resolved `Theme`.
3. The private generic `run_with` receives the context plus a distinct
   initialization callback, invokes initializer and body with the same mutable
   context in sequence, and keeps the existing total cleanup scope around both
   phases.
4. `app.rs` supplies only `state.status`, a context containing its existing
   `&mut AppState` and `&mut TextArea`, the exact existing first-frame render,
   and the renderer-loop body. It no longer imports or invokes
   `initialize_terminal_presentation`.
5. No new terminal, thread, channel, cancellation token, state cache, cleanup
   guard, or compatibility path is added.

## Frozen Behavior

### Normal Startup, Run, And Exit

1. `PendingTerminalSession::start`, input capability probing, theme resolution,
   channel construction, hosted-agent startup, and ratatui activation remain
   unchanged.
2. After activation, the owner attempts the pending title first, then draws the
   exact current `ui::render` frame once, then enters `RendererLoopOwner`.
3. The renderer loop receives the same terminal, presentation, input-wake,
   theme, state, configuration, queues, prompt, transcript, and workspace
   collaborators as before.
4. Normal exit preserves the exact renderer exit code and the existing cleanup
   order: reset title, drop ratatui, finish input, stop renderer runtime, close
   runtime inbox, then cancel and join the hosted agent/runtime host.

### Failure And Error Precedence

1. A pending-title write remains best-effort and cannot prevent first draw.
2. A first-draw error prevents renderer-loop entry and becomes the active
   terminal result.
3. Initialization failure still runs reset, terminal drop, and input finish.
   That initialization error remains primary over reset or input-finish errors,
   matching the existing body-error precedence.
4. A later renderer error preserves its exact value and follows the same total
   terminal and post-terminal shutdown path.
5. Activation failure remains outside initialization but still reaches
   `finish_tui_run`, preserving the preceding slice's explicit runtime shutdown
   contract.
6. Cleanup does not retry, aggregate errors, fabricate TUI events, or alter the
   current best-effort title-write normalization.

### Cancellation, Rejection, Timeout, Retry, Disconnect, And Restart

1. Cancellation, approval rejection, user-input settlement, operation timeout,
   retry, compaction, and recovery begin only after renderer-loop entry and are
   unchanged.
2. No input is consumed by the renderer before initial presentation completes.
   Existing qwertty buffering and bounded input channels remain active during
   first draw.
3. Runtime-event and interaction-ack disconnect behavior remains in the
   existing renderer owners; no channel capacity or close order changes.
4. Restart creates a fresh pending and active terminal session, performs the
   same mandatory first presentation, then hydrates the same runtime-owned
   typed snapshot.
5. Registry-only legacy tasks remain hidden and non-actionable. Their cold
   migration requires a separate runtime-owned Spec with durable identity,
   operation, interaction, ownership, and rollback semantics.

## Ownership And Compatibility

- `terminal_session.rs` owns pending/active physical terminal lifetime,
  mandatory initial presentation, renderer-body admission, input-wake lifetime,
  and terminal/input cleanup.
- `presentation.rs` retains the generic title-before-draw and total cleanup
  primitives.
- `renderer_loop.rs` remains the sole foreground iteration owner.
- `app.rs` retains application state, hosted runtime and renderer collaborator
  construction, the concrete first-frame UI render, and `TuiExit` construction.
- `tui_run_lifecycle.rs` retains post-terminal shutdown order and error
  precedence.
- The runtime surface remains the unique live task fact source. `TaskRegistry`
  is not read by the TUI bootstrap path.
- No CLI, TUI command/key flow, server/JSONL, app-server, ACP, public Rust API,
  history, task registry, surface ledger, or stored schema changes.

## Validator Contract

Extend the closed `terminal_session_lifecycle` entrypoint rather than adding a
parallel manifest boundary. Path-specific anchors must prove:

1. `app.rs` passes initial status and the first-frame render callback into the
   active owner and has no direct `initialize_terminal_presentation` call.
2. `terminal_session.rs` invokes `initialize_terminal_presentation` before the
   renderer body inside the cleanup scope.
3. The production title callback uses `write_pending`, the draw callback uses
   the resolved theme, and initialization failure remains under the existing
   cleanup boundary.

Negative validator self-tests must delete or reorder the owner initialization
anchor and must not pass because of imports, presentation unit tests, or
unrelated renderer calls. Mutation and harmless-method baselines must not be
broadened.

## Test Strategy

1. Add an active-owner test that calls the wished-for `run_with` initialization
   seam. Verify RED because the method does not yet accept a separate
   initializer.
2. In GREEN, prove successful initialization mutates one shared context before
   the renderer body observes it, and a body failure still produces exact
   `initialize, body, reset, drop, finish` order.
3. Add a second direct case in the same test module proving initialization
   failure skips the body, still produces exact
   `initialize, reset, drop, finish` order, and retains the initialization
   error.
4. Move production initialization into `ActivatedTerminalSession::run`, remove
   the direct app import/call, and keep presentation, active-owner, renderer,
   input-runtime, and PTY behavior green.
5. Update the closed validator and self-tests, regenerate the manifest digest,
   update this Spec and the roadmap, and run focused/full gates before and after
   rebase and on integrated local `main`.

## Acceptance Criteria

1. `app.rs` neither imports nor directly calls
   `initialize_terminal_presentation`.
2. Exactly one active-terminal production method enforces initial pending
   title, first draw, renderer body, and total cleanup order while lending one
   mutable renderer context sequentially to first draw and renderer body.
3. Direct behavior tests prove both successful initialization ordering and
   initialization-failure cleanup/error precedence.
4. The initial frame, renderer inputs, normal exit code, activation failure,
   body failure, and post-terminal shutdown behavior remain unchanged.
5. No registry task list, legacy approval, typed surface state, external
   protocol, or persisted format is changed or mirrored.
6. Closed validator anchors and deletion self-tests cover the production seam
   without a new compatibility layer or second owner.
7. Focused/full TUI, PTY, validator, formatter, and diff gates pass, and review
   has no unresolved Critical or Important issue.
8. After local-main integration and root verification, remove only this slice's
   worktree and topic branch immediately.

## Verification Commands

```bash
cargo test -p orca-tui terminal_session --lib --locked -- --test-threads=1
cargo test -p orca-tui presentation --lib --locked -- --test-threads=1
cargo test -p orca-tui renderer_loop --lib --locked -- --test-threads=1
cargo test -p orca-tui input_runtime --lib --locked -- --test-threads=1
cargo check -p orca-tui --tests --locked
cargo test -p orca-tui --lib --locked -- --test-threads=1
cargo test --test tui_pty_contract --locked -- --test-threads=1
node scripts/validate-runtime-surface-contract.mjs
node --test scripts/test-validate-runtime-surface-contract.mjs
node scripts/validate-windows-platform-boundaries.mjs
node --test scripts/test-validate-windows-platform-boundaries.mjs
cargo fmt --all -- --check
git diff --check
rg -n 'initialize_terminal_presentation' crates/orca-tui/src/app.rs
```

The final search must be empty.

## Implementation Evidence

- Owner RED failed to compile with `E0593`/`E0061` because `run_with` had no
  distinct initialization callback. GREEN adds the callback and proves exact
  `initialize, body, reset, drop, finish` success/body-error order plus exact
  `initialize, reset, drop, finish` initialization-failure order.
- The first production migration exposed overlapping mutable closure captures
  as `E0499`/`E0502`. The final owner holds one generic context and lends it
  sequentially to initial draw and renderer body; no lock, unsafe escape, or
  second state authority was added.
- Focused filters pass: terminal session 4/4, presentation 25/25, renderer loop
  3/3, and input runtime 13/13. The test compiler gate also passes with only
  existing warnings.
- `cargo test -p orca-tui --lib --locked -- --test-threads=1` passes
  1,150/1,150, and the root-package `tui_pty_contract` target passes 6/6.
- Runtime-surface and Windows-boundary validators and both self-test suites
  pass. The negative fixtures remove owner initialization, status/context,
  first draw, pending title, draw delegation, and revive the forbidden app
  initializer; each is rejected by the intended path-specific boundary.
- `cargo fmt --all -- --check`, `git diff --check`, and the obsolete app-path
  search pass. The manifest SHA-256 in the digest matches the manifest bytes.
- CodeRabbit 0.7.3 reviewed all eight staged implementation/contract files and
  reported 0 findings. Manual source audit found no second terminal authority,
  changed collaborator, or compatibility drift.
- Fetch confirmed `origin/main` had not advanced; rebase onto local `main` was
  a no-op. Focused/compiler/mechanical gates, both validator suites, the full
  1,150-test TUI suite, and the 6-test PTY contract passed again afterward.
- Local `main` fast-forwarded cleanly and passed the same focused/compiler,
  validator, 1,150-test TUI, and 6-test PTY gates from the root checkout. The
  slice worktree and topic branch were then removed immediately; all unrelated
  worktrees remained registered.

## Migration, Rollback, And Old-Path Deletion

Migration order is owner RED, owner GREEN, production call migration, direct
app-path deletion, validator closure, focused/full verification, review, one
semantic commit, rebase, local-main integration, root verification, and
immediate worktree/branch cleanup. Reverting that commit restores the prior
internal call composition without data migration. No push, tag, GitHub Release,
npm publication, or remote cleanup is authorized.

## Spec Self-Review

The slice defines normal startup, first presentation, error precedence,
cancellation, rejection, timeout, retry, disconnect, restart, ownership,
compatibility, validation, deletion, rollback, and cleanup. It creates no
second terminal or task authority and is independently
implementable, verifiable, committable, and reversible.
