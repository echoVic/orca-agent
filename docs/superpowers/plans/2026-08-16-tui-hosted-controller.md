# TUI Hosted Controller Ownership Plan

**Goal:** Move the final hosted action receive/lifecycle loop out of the
renderer module without changing valid runtime behavior or compatibility, and
reject malformed fallback model actions instead of panicking.

**Architecture:** Add one crate-private `hosted_controller` owner for startup
restoration, Side command restrictions, typed action dispatch, and controller
exit cleanup. `app.rs` keeps terminal/frame ownership and runtime construction;
existing focused hosted modules keep all action transactions and mutation.

**Tech stack:** Rust, crossbeam channels, runtime host/surface APIs, Cargo
tests, Node contract validators.

## Task 1: Spec Gate And RED Owner Test

- [x] Audit the full controller body, production/test callers, module
  dependencies, action anchors, direct mutation baseline, downstream tests,
  roadmap evidence, and source counts.
- [x] Write the Proposed spec before production edits.
- [x] Add `hosted_controller` to the crate and a direct owner test through the
  absent controller entry point.
- [x] Run the exact owner test RED because the entry point does not exist.

## Task 2: Relocate The Controller

- [x] Move `hosted_tui_controller_loop` semantically into
  `hosted_controller.rs`, adding imports and crate-private visibility, then
  apply the review-driven malformed-model rejection repair.
- [x] Make `app.rs` import and call the new owner; remove production imports
  used only by the moved loop while retaining test-only imports.
- [x] Keep renderer test harnesses in `app.rs` and preserve their real runtime
  path through the moved controller.
- [x] Run the direct owner test GREEN and the focused controller/hosted action
  suites.
- [x] Reproduce the malformed-model panic RED, then prove exact rejection and
  a successful follow-up dispatch GREEN.

## Task 3: Freeze The Boundary

- [x] Move controller-side action and entrypoint anchors to
  `hosted_controller.rs` while keeping `app.rs` anchored as production caller.
- [x] Move the one controller thread-shutdown mutation baseline to the new
  owner and add deletion-resistant validator self-tests.
- [x] Update the manifest/digest, roadmap owner inventory/counts, implemented
  spec evidence, and this plan.
- [x] Run compiler check, runtime and Windows validators plus self-tests,
  formatter, and diff checks.

## Task 4: Review, Integrate, And Clean Up

- [x] Run the full serial TUI library suite and root-package PTY contract.
- [x] Request independent review and resolve every Critical or Important
  finding.
- [x] Commit once as `fix(tui): own hosted controller`.
- [x] Rebase onto latest local `main` and repeat affected and full gates.
- [x] Fast-forward local `main`, repeat root full gates, then immediately remove
  only this worktree and merged topic branch.
