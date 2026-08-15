# TUI Hosted Goal Action Ownership Plan

**Goal:** Give all six hosted Goal action transactions one focused owner while
preserving public commands, runtime surface facts, event order, lifecycle, and
continuation behavior.

**Architecture:** Add a crate-private `HostedGoalAction` command and
`handle_hosted_goal_action` entry point in `hosted_goal.rs`. The controller maps
existing `UserAction` variants into that boundary. Existing session lifecycle,
projection, runtime surface action, and recovery owners remain authoritative.

**Tech stack:** Rust, crossbeam channels, `orca-tui`, runtime surface actions,
Cargo tests, Node contract validators.

## Task 1: Spec Gate And RED Ownership Test

- [x] Audit the current Goal match arms, module owners, behavioral tests,
  validator anchors, and measured source sizes.
- [x] Write the proposed spec before production edits.
- [x] Add a direct `hosted_goal` test for empty recorded-session `Show` through
  the new owner API.
- [x] Run the focused test and confirm RED because the owner API is absent.

## Task 2: Extract The Goal Action Transaction

- [x] Add `HostedGoalAction` and `handle_hosted_goal_action` to
  `hosted_goal.rs`.
- [x] Move the six controller bodies with unchanged ordering, messages,
  timestamp semantics, and recovery delegation.
- [x] Reduce the controller Goal arms to command mapping and remove imports or
  helpers made unused by the extraction.
- [x] Run the direct owner test and focused Goal/controller regressions GREEN.

## Task 3: Documentation, Review, And Gates

- [x] Update roadmap, implemented spec status, measured counts, source
  ownership evidence, manifest references, and validator self-tests as needed.
- [x] Run `cargo check -p orca-tui --tests --locked`, full serial TUI, PTY,
  runtime and Windows validators/self-tests, formatter, and diff checks.
- [x] Request independent review and resolve every Critical/Important finding.
- [x] Commit once as `refactor(tui): own hosted goal actions`.
- [x] Rebase onto latest local `main` and repeat affected gates.
- [x] Fast-forward local `main`, repeat root gates, and remove only this
  worktree/branch.
