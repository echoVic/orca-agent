# Hosted Submission Ownership Plan

**Goal:** Move submitted-turn orchestration out of `app.rs` while preserving
thread startup, queued rejection identity, mention expansion, runtime dispatch,
event order, and Goal behavior.

## File Map

- `crates/orca-tui/src/hosted_submission.rs`: own the submitted-turn
  transaction and focused behavior tests.
- `crates/orca-tui/src/app.rs`: retain controller routing and call the
  submission owner.
- `crates/orca-tui/src/lib.rs`: register the module.
- `docs/production-roadmap.md`: record the owner and fresh line counts.
- `docs/superpowers/specs/2026-08-16-tui-hosted-submission.md`: track status and
  behavioral evidence.

## Task 1: Freeze Behavior With RED Tests

- [x] Add focused tests for startup rejection and queued mention rejection.
- [x] Run the hosted-submission filter and confirm failure before relocation.

## Task 2: Extract Hosted Submission

- [x] Move `handle_hosted_submitted_turn` without changing its body, arguments,
  ordering, error shaping, or notification behavior.
- [x] Update controller call sites and keep the helper crate-visible only.
- [x] Run focused submission, workflow-notification, recovery, Goal, and compile
  checks.

## Task 3: Documentation, Review, And Gates

- [x] Update roadmap and implemented spec status with fresh line counts and
  actual gate evidence.
- [x] Run full TUI, PTY, validators, formatter, and diff checks.
- [x] Request independent review and fix Critical/Important findings.
- [x] Commit once as `refactor(tui): own hosted submission`.
- [x] Rebase onto latest local `main` and repeat affected gates.
- [x] Fast-forward local `main`, repeat root gates, and remove only this
  worktree/branch.
