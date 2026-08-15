# Hosted Session Lifecycle Ownership Plan

**Goal:** Move hosted session start/preflight/install/reap transactions out of
`app.rs` while preserving runtime identity, replacement, and shutdown behavior.

## File Map

- `crates/orca-tui/src/hosted_session_lifecycle.rs`: own hosted lifecycle
  transaction helpers.
- `crates/orca-tui/src/app.rs`: retain controller loop, attachments, Goal
  recovery, and call the lifecycle module APIs.
- `crates/orca-tui/src/lib.rs`: register the module.
- `scripts/validate-runtime-surface-contract.mjs`: move the reviewed lifecycle
  call-site inventory to the new owner path without changing the protocol
  manifest.
- `docs/production-roadmap.md`: record the owner and fresh line counts.
- `docs/superpowers/specs/2026-08-16-tui-hosted-session-lifecycle.md`: track
  status and evidence.

## Task 1: Freeze Behavior With RED Tests

- [x] Add a module test proving no current session is switchable, plus preserve
  the existing integration acceptance tests.
- [x] Run the focused lifecycle test and confirm failure before relocation.

## Task 2: Extract Lifecycle Transactions

- [x] Move the listed start/preflight/install/reap/switch/list helpers into
  `hosted_session_lifecycle.rs`.
- [x] Update controller and test call sites without changing error text,
  replacement ordering, or reaper behavior.
- [x] Run focused lifecycle/preflight/fork tests and `cargo check -p orca-tui --tests --locked`.

## Task 3: Documentation, Review, And Gates

- [x] Update roadmap and implemented spec status with fresh line counts.
- [x] Run focused tests, full TUI, PTY, validators, formatter, and diff checks.
- [x] Request independent review and fix Critical/Important findings.
- [x] Commit once as `refactor(tui): own hosted session lifecycle`.
- [x] Rebase onto latest local `main` and repeat affected gates.
- [x] Fast-forward local `main`, repeat root gates, and remove only this
  worktree/branch.
