# TUI Hosted Side Lifecycle Action Ownership Plan

**Goal:** Give Side start/toggle/close one lifecycle owner while preserving
candidate rollback, attachment fencing, projection order, background
presentation rebind, and bounded shutdown behavior.

**Architecture:** Add a crate-private `HostedSideAction` and
`handle_hosted_side_action` in `hosted_side.rs`. The controller maps the three
existing `UserAction` variants into the owner. Move the generic attached-sender
rotation helper to `attachment_routing.rs` so Side can depend on session
preflight/reaping without introducing a production module cycle.

**Tech stack:** Rust, crossbeam channels, runtime surface actions, Cargo tests,
Node contract validators.

## Task 1: Spec Gate And RED Owner Test

- [x] Audit the three controller branches, Side/session/attachment owners,
  success and rollback ordering, behavioral tests, manifest rows, and counts.
- [x] Write the proposed spec before production edits.
- [x] Add a direct `hosted_side` Start-without-parent test through the absent
  owner API.
- [x] Run it RED because `HostedSideAction` and the handler do not exist.

## Task 2: Extract The Side Lifecycle Transaction

- [x] Move `rotate_attached_event_sender` to `attachment_routing.rs` without
  changing generation or routing behavior.
- [x] Add `HostedSideAction` and `handle_hosted_side_action`.
- [x] Move start/toggle/close with unchanged rollback, projection, routing,
  rebind, event, and shutdown ordering.
- [x] Reduce controller branches to command mapping and scope imports/helpers
  made test-only by the move.
- [x] Run the direct owner and focused Side/routing/controller tests GREEN.

## Task 3: Documentation, Review, And Gates

- [x] Update roadmap, implemented spec, counts, manifest/digest references, and
  deletion-resistant validator self-tests.
- [x] Run `cargo check -p orca-tui --tests --locked`, full serial TUI, PTY,
  runtime/Windows validators and self-tests, formatter, and diff checks.
- [x] Request independent review and resolve every Critical/Important finding.
- [x] Commit once as `refactor(tui): own hosted side actions`.
- [x] Rebase onto latest local `main` and repeat affected gates.
- [x] Fast-forward local `main`, repeat root gates, and remove only this
  worktree/branch.
