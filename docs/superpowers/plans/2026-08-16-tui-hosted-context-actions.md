# TUI Hosted Context Action Ownership Plan

**Goal:** Give Remember, Compact, and Backtrack one hosted transaction owner
while preserving typed runtime authority and exact user-visible event/error
ordering.

**Architecture:** Add private `hosted_context.rs` with a crate-private
`HostedContextAction` and `handle_hosted_context_action`. The controller maps
the three existing `UserAction` variants into that owner; memory, compaction,
and history mutations remain behind `TuiSurfaceActions`.

**Tech stack:** Rust, crossbeam channels, runtime surface actions, Cargo tests,
Node contract validators.

## Task 1: Spec Gate And RED Owner Test

- [x] Audit controller branches, runtime owners, success/failure ordering,
  behavioral tests, manifest rows, module dependencies, and source counts.
- [x] Write the proposed spec before production edits.
- [x] Add a direct Compact/Backtrack-without-thread test through the absent
  owner API.
- [x] Run it RED because `HostedContextAction` and the handler do not exist.

## Task 2: Extract The Hosted Context Transaction

- [x] Add the private module, `HostedContextAction`, and handler.
- [x] Move Remember/Compact/Backtrack with unchanged startup, mutation, event,
  error, cancellation, and prompt-restoration ordering.
- [x] Reduce controller branches to command mapping and scope imports/helpers
  made test-only by the move.
- [x] Run direct owner and focused memory/compaction/backtrack tests GREEN.

## Task 3: Documentation, Review, And Gates

- [x] Update roadmap, implemented spec, counts, manifest/digest references, and
  deletion-resistant validator self-tests.
- [x] Run compiler check, full serial TUI, PTY, runtime/Windows validators and
  self-tests, formatter, and diff checks.
- [x] Request independent review and resolve every Critical/Important finding.
- [x] Commit once as `refactor(tui): own hosted context actions`.
- [x] Rebase onto latest local `main` and repeat affected gates.
- [x] Fast-forward local `main`, repeat root gates, and remove only this
  worktree/branch.
