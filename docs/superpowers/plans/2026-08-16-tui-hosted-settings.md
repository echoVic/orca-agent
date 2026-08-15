# Hosted Settings Ownership Plan

**Goal:** Move hosted settings translation and application out of `app.rs`
while preserving typed runtime authority, startup-only config behavior, event
ordering, and plan-implementation gating.

## File Map

- `crates/orca-tui/src/hosted_settings.rs`: own settings patch translation and
  the attached/unattached application transaction.
- `crates/orca-tui/src/app.rs`: retain controller routing and call the settings
  owner.
- `crates/orca-tui/src/lib.rs`: register the module.
- `docs/production-roadmap.md`: record the owner and fresh line counts.
- `docs/superpowers/specs/2026-08-16-tui-hosted-settings.md`: track status and
  behavioral evidence.

## Task 1: Freeze Behavior With RED Tests

- [x] Add focused module tests for patch order, unattached settings/event
  application, attached runtime commit/mirroring, and sessionless attached
  rejection without local mutation.
- [x] Run the focused module filter and confirm failure before relocation.

## Task 2: Extract Hosted Settings

- [x] Move the three helpers into `hosted_settings.rs` without changing bodies,
  patch order, rejection text, or event order.
- [x] Update controller call sites and keep only externally used helpers
  crate-visible.
- [x] Run focused settings, plan, and compile checks.

## Task 3: Documentation, Review, And Gates

- [x] Update roadmap and implemented spec status with fresh line counts and
  actual gate evidence.
- [x] Run full TUI, PTY, validators, formatter, and diff checks.
- [x] Request independent review and fix Critical/Important findings.
- [x] Commit once as `refactor(tui): own hosted settings`.
- [x] Rebase onto latest local `main` and repeat affected gates.
- [x] Fast-forward local `main`, repeat root gates, and remove only this
  worktree/branch.
