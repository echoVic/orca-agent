# Hosted Goal Orchestration Ownership Plan

**Goal:** Move hosted Goal and typed-turn orchestration out of `app.rs` while
preserving the existing runtime behavior and TUI event contract.

## File Map

- `crates/orca-tui/src/hosted_runtime.rs`: own typed turn request construction,
  ordinary-turn invocation, submission rejection, and shared operation error/
  terminal shaping.
- `crates/orca-tui/src/hosted_goal.rs`: own Goal continuation, Goal lookup,
  display, history errors, and run orchestration.
- `crates/orca-tui/src/app.rs`: remove duplicate helpers and route controller
  and submitted-turn call sites through the module owners; retain
  latest-active recovery with controller-owned thread installation/shutdown.
- `docs/production-roadmap.md`: record the renderer-orchestration owner and
  fresh line counts.
- `docs/superpowers/specs/2026-08-16-tui-goal-orchestration.md`: track status
  and acceptance evidence.

## Task 1: Freeze Behavior With Focused RED Coverage

- [x] Add hosted-runtime unit coverage for `HostedTurnRequest` flags,
  backtrack/task-description propagation, workflow-notification semantics, and
  terminal-error shaping.
- [x] Run the focused RED test before relocation; it failed because the helper
  was still private to `app.rs`.

## Task 2: Extract Stateless Hosted Runtime Helpers

- [x] Move `hosted_turn_request`, `run_hosted_ordinary_turn`,
  `send_hosted_operation_terminal_failure`, and
  `emit_hosted_operation_error` plus `send_submission_error` into
  `hosted_runtime.rs`.
- [x] Update submitted-turn, workflow, Goal, and test call sites without
  changing event order or error text.
- [x] Run `cargo check -p orca-tui --tests --locked`.

## Task 3: Extract Goal Orchestration

- [x] Move Goal continuation prompt/session-id lookup, Goal display/history
  errors, and `run_hosted_goal_run` into `hosted_goal.rs`.
- [x] Keep controller-owned `thread`, `preloaded`, config, event sender, and
  cancellation handles explicit parameters; do not add a second state store.
- [x] Keep latest-active Goal recovery in `app.rs` because it owns the
  runtime-thread installation/shutdown and preloaded-session transaction.
- [x] Run Goal-focused and Side/task-control suites.

## Task 4: Documentation, Review, And Gates

- [x] Update roadmap and implemented spec status with fresh line counts.
- [x] Run focused tests, full TUI, PTY, validators, formatter, and diff checks.
- [x] Request independent review; no Critical/Important findings required fixes.
- [x] Commit once as `refactor(tui): own hosted goal orchestration`.
- [x] Rebase onto latest local `main` and repeat affected gates.
- [x] Fast-forward local `main`, repeat root gates, and remove only this
  worktree/branch.
