# Hosted Session Projection Ownership Plan

**Goal:** Move stateless hosted snapshot/history event shaping out of `app.rs`
while preserving attachment, ordering, and session lifecycle behavior.

## File Map

- `crates/orca-tui/src/hosted_session.rs`: own stateless snapshot/history
  projection and startup eligibility helpers.
- `crates/orca-tui/src/app.rs`: retain controller/session transactions and call
  the module APIs.
- `crates/orca-tui/src/lib.rs`: register the module.
- `docs/production-roadmap.md`: record the new owner and line counts.
- `docs/superpowers/specs/2026-08-16-tui-hosted-session-projection.md`: track
  status and evidence.

## Task 1: Freeze Behavior With RED Tests

- [x] Add module tests for UUID/startup eligibility, empty-history emission, and
  attached reset/history ordering.
- [x] Run the focused tests and confirm failure before relocation.

## Task 2: Extract Stateless Session Projection Helpers

- [x] Move snapshot conversion, attached projection, runtime-ready publication,
  history fallback, startup eligibility, UUID recognition, and empty-history
  emission into `hosted_session.rs`.
- [x] Update all controller and test call sites without changing event order or
  error text.
- [x] Run focused session/Side tests and `cargo check -p orca-tui --tests --locked`.

## Task 3: Documentation, Review, And Gates

- [x] Update roadmap and implemented spec status with fresh line counts.
- [x] Run focused tests, full TUI, PTY, validators, formatter, and diff checks.
- [x] Request independent review and fix Critical/Important findings.
- [ ] Commit once as `refactor(tui): own hosted session projection`.
- [ ] Rebase onto latest local `main` and repeat affected gates.
- [ ] Fast-forward local `main`, repeat root gates, and remove only this
  worktree/branch.
