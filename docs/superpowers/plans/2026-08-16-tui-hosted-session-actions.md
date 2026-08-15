# TUI Hosted Session Action Ownership Plan

**Goal:** Give the eight hosted main-session actions one lifecycle owner while
preserving attachment, projection, failure, persistence, and shutdown behavior.

**Architecture:** Add a crate-private `HostedSessionAction` and
`handle_hosted_session_action` in `hosted_session_lifecycle.rs`. The controller
maps existing `UserAction` variants into the boundary. Existing start/preflight/
install/reap helpers, attachment routing, session projection, and typed host/
surface actions remain authoritative.

**Tech stack:** Rust, crossbeam channels, runtime surface actions, Cargo tests,
Node contract validators.

## Task 1: Spec Gate And RED Owner Test

- [x] Audit the eight controller branches, lifecycle APIs, behavioral tests,
  failure semantics, validator anchors, and source sizes.
- [x] Write the proposed spec before production edits.
- [x] Add a direct lifecycle-module `RenameCurrent` test through the absent
  owner API.
- [x] Run it RED because `HostedSessionAction` and the handler do not exist.

## Task 2: Extract The Session Action Transaction

- [x] Add `HostedSessionAction` and `handle_hosted_session_action`.
- [x] Move all eight branch bodies with unchanged event/error ordering,
  attachment rotation, history presentation, and current-session protection.
- [x] Reduce controller branches to command mapping and scope imports/helpers
  made test-only by the move.
- [x] Run the direct owner and focused session/controller regressions GREEN.

## Task 3: Documentation, Review, And Gates

- [x] Update roadmap, implemented spec, counts, manifest/digest references, and
  deletion-resistant validator self-tests as required.
- [x] Add the review-driven recorded current-session archive/delete guard
  regression and correct the pre-review coverage claim.
- [x] Run `cargo check -p orca-tui --tests --locked`, full serial TUI, PTY,
  runtime/Windows validators and self-tests, formatter, and diff checks.
- [x] Request independent review and resolve every Critical/Important finding.
- [x] Commit once as `refactor(tui): own hosted session actions`.
- [x] Rebase onto latest local `main` and repeat affected gates.
- [x] Fast-forward local `main`, repeat root gates, and remove only this
  worktree/branch.
