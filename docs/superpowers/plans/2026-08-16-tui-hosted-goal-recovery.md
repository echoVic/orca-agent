# Hosted Latest-Active Goal Recovery Ownership Plan

**Goal:** Move latest-active Goal session recovery into the hosted session
lifecycle owner while preserving candidate validation, replacement ordering,
and continuation behavior.

## File Map

- `crates/orca-tui/src/hosted_session_lifecycle.rs`: own the recovery
  transaction and its focused disabled-history test.
- `crates/orca-tui/src/app.rs`: retain action selection, call the lifecycle
  API, and keep controller/Side/final shutdown ownership.
- `docs/production-roadmap.md`: record the new lifecycle owner and line counts.
- `docs/superpowers/specs/2026-08-16-tui-hosted-goal-recovery.md`: record
  implementation and evidence.
- `scripts/validate-runtime-surface-contract.mjs`: move the reviewed helper
  inventory to the lifecycle owner without changing the protocol schema or
  mutation counts.

## Task 1: Freeze Behavior With RED Tests

- [x] Add the lifecycle-module disabled-history behavior test.
- [x] Run the focused lifecycle test and confirm the new owner API is absent
  before relocation.

## Task 2: Extract Recovery Transaction

- [x] Move `resume_latest_active_goal_hosted` verbatim apart from imports and
  crate-private visibility.
- [x] Update the controller call site and remove the old app definition.
- [x] Run lifecycle, successful-recovery, no-goal, and restart-focused tests.

## Task 3: Documentation, Review, And Gates

- [x] Update roadmap and implemented spec with fresh counts and evidence.
- [x] Run focused tests, `cargo check -p orca-tui --tests --locked`, full TUI,
  PTY, validators, formatter, and diff checks.
- [x] Request independent review and fix Critical/Important findings.
- [x] Commit once as `refactor(tui): own hosted goal recovery`.
- [x] Rebase onto latest local `main` and repeat affected gates.
- [x] Fast-forward local `main`, repeat root gates, and remove only this
  worktree/branch.
