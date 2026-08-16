# TUI Terminal Lifecycle Ownership Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> `superpowers:executing-plans` to implement this plan task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the activated terminal session unique ownership of its renderer
scope and total cleanup without changing normal TUI behavior or external
contracts.

**Architecture:** Retain terminal, presentation, input runtime, resolved theme,
and receivers inside a generic private `ActivatedTerminalSession`. Its
production `run` method constructs the existing input-wake owner and delegates
to a directly testable generic cleanup method. Totalize the existing
presentation finish primitive so reset failure cannot skip later cleanup.

**Tech Stack:** Rust, ratatui, crossterm, qwertty, crossbeam-channel, Cargo
tests, Node contract validators.

---

### Task 1: Spec Gate And RED Cleanup Test

**Files:**

- Create: `docs/superpowers/specs/2026-08-16-tui-terminal-lifecycle-ownership.md`
- Create: `docs/superpowers/plans/2026-08-16-tui-terminal-lifecycle-ownership.md`
- Modify: `crates/orca-tui/src/presentation.rs`

- [x] Audit current pending/activated ownership, normal and failed cleanup,
  input-runtime Drop behavior, app shutdown order, tests, validators, recent
  changes, roadmap, and source counts.
- [x] Confirm clean base `cb7d15d6c`, fetch remote, prove `origin/main` is not
  ahead, verify `.worktrees` is ignored, and create isolated
  `codex/tui-terminal-lifecycle-owner`.
- [x] Write and self-review this Proposed spec and plan with no placeholder,
  second terminal owner, new state source, or undefined error precedence.
- [x] Add `presentation::tests::reset_failure_still_drops_terminal_and_finishes_input`
  using shared call recording. Make reset and finish both fail; assert calls are
  exactly `reset`, `drop`, `finish` and the returned error is exactly the reset
  error.
- [x] Run
  `cargo test -p orca-tui presentation::tests::reset_failure_still_drops_terminal_and_finishes_input --lib --locked -- --exact --test-threads=1`.
  Expected RED: the call assertion reports only `reset` because current `?`
  returns before drop and finish.

### Task 2: Totalize Presentation Cleanup

**Files:**

- Modify: `crates/orca-tui/src/presentation.rs`
- Modify: `crates/orca-tui/src/app.rs` (test relocation only)

- [x] Replace the early `?` with a recorded reset result, always invoke the
  injected terminal-drop callback, always invoke input finish, and return reset
  error first or finish result otherwise.
- [x] Run the exact RED test GREEN.
- [x] Move the successful title/draw, resume, reset/drop/finish, and
  cleanup-after-body-error tests from `app::tests` to `presentation::tests` so
  the primitive owner contains its behavior evidence; do not change assertions.
- [x] Run `cargo test -p orca-tui presentation --lib --locked -- --test-threads=1`
  and require every presentation test to pass.

### Task 3: RED Activated-Session Owner Test

**Files:**

- Modify: `crates/orca-tui/src/terminal_session.rs`

- [x] Add a direct module test that constructs the wished-for generic
  `ActivatedTerminalSession<Vec<&str>, Vec<&str>>`, real bounded event/focus/
  control receivers, a real `TerminalPresentation`, and a dark `Theme`.
- [x] Call the absent `run_with` API with a queued key event. In the body,
  receive that event through the supplied `RendererInputWakeOwner`, then return
  `io::Error("body failed")`. Inject reset/drop/finish callbacks and assert
  exact `body`, `reset`, `drop`, `finish` order plus exact body-error precedence.
- [x] Run `cargo test -p orca-tui activated_session --lib --locked -- --test-threads=1`.
  Expected RED: compile failure for absent generic activated fields/method.

### Task 4: Implement Active Terminal Ownership And Delete App Path

**Files:**

- Modify: `crates/orca-tui/src/terminal_session.rs`
- Modify: `crates/orca-tui/src/app.rs:273-337`

- [x] Make `ActivatedTerminalSession<Terminal = InlineTerminal, Input = InputRuntime>`
  retain terminal, presentation, and input as fields instead of exposing
  `into_parts`.
- [x] Add private generic `run_with` that constructs exactly one
  `RendererInputWakeOwner`, runs the body inside
  `with_terminal_presentation_cleanup`, and delegates cleanup to
  `finish_terminal_presentation` with injected reset/drop/finish operations.
- [x] Add the production-specialized consuming `run` that binds the existing
  best-effort title reset to the live terminal backend, terminal retirement to
  `drop`, and input cleanup to `InputRuntime::finish`.
- [x] Replace app's `into_parts`, input-wake construction, and cleanup closures
  with `pending_terminal_session.activate()?.run(...)`. Preserve the exact
  initial title/draw and `RendererLoopOwner` body.
- [x] Remove obsolete app imports and direct tests. Do not move renderer or
  hosted-runtime shutdown.
- [x] Run activated-session, presentation, terminal-session, renderer-input-
  wake, renderer-loop, and input-runtime filters GREEN.

### Task 5: Totalize Post-Terminal Runtime Cleanup

**Files:**

- Create: `crates/orca-tui/src/tui_run_lifecycle.rs`
- Modify: `crates/orca-tui/src/lib.rs`
- Modify: `crates/orca-tui/src/app.rs:337-340`

- [x] Register `mod tui_run_lifecycle;` and add a direct test importing the
  absent `finish_tui_run`. Give it an exact renderer error, make agent shutdown
  fail independently, and record all cleanup callbacks.
- [x] Run `cargo test -p orca-tui tui_run_lifecycle --lib --locked -- --test-threads=1`.
  Expected RED: unresolved import/function only.
- [x] Implement `finish_tui_run` to call renderer shutdown, inbox close, and
  agent shutdown exactly once in order. Return the renderer/terminal error when
  present; otherwise return the agent result with the renderer value.
- [x] Replace app's success-only shutdown sequence with one call using the
  still-owned renderer, inbox, and agent owners. Require exact RED test GREEN.
- [x] Add the successful-result/failed-agent case and require the exact agent
  error after all cleanup callbacks.
- [x] After broad final review exposed activation's remaining early `?`, first
  tighten the lifecycle anchor and observe validator RED, then match activation
  into `renderer_result` so activation failure also reaches all three shutdown
  callbacks with its exact error preserved.

### Task 6: Close Contracts, Evidence, Review, And Integration

**Files:**

- Modify: `crates/orca-tui/src/surface_boundary_tests.rs`
- Modify: `scripts/validate-runtime-surface-contract.mjs`
- Modify: `scripts/test-validate-runtime-surface-contract.mjs`
- Modify: `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json`
- Modify: `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.digest.json`
- Modify: `docs/production-roadmap.md`
- Modify this spec and plan with measured evidence.

- [x] Add closed `terminal_session_lifecycle` app/owner/presentation anchors,
  migrate renderer-input-wake construction from app to terminal session, anchor
  the post-terminal coordinator, and add deletion self-tests for every
  production/order/error anchor.
- [x] Regenerate the manifest SHA-256 and update only its digest entry. Update
  roadmap boundary inventory, ownership wording, source counts, and next
  evidence-based boundary.
- [x] Run ordinary/test compiler gates, both validators and self-tests,
  formatter, digest equality, diff check, focused suites, full serial TUI, and
  root PTY.
- [x] Stage the exact slice files and run CodeRabbit over the complete staged
  diff; resolve every valid Critical or Important finding.
- [x] Create one semantic commit:
  `refactor(tui): own active terminal lifecycle`.
- [x] Rebase latest local `main`; repeat affected/validator/full/PTY gates.
- [x] Fast-forward local `main`; repeat root affected/validator/full/PTY gates.
- [x] Immediately remove only `.worktrees/tui-terminal-lifecycle-owner` and
  `codex/tui-terminal-lifecycle-owner`, preserve unrelated worktrees, record
  final evidence in the same commit, and verify clean root state.
