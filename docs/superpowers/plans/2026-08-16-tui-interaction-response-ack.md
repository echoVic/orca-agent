# TUI Interaction Response Acknowledgement Plan

**Goal:** Preserve admitted pending user/MCP interaction input until the runtime
response is committed, while keeping stale responses terminal.

**Architecture:** Add a crate-private bounded non-blocking dispatcher acknowledgement lane,
store one key-matched composer snapshot in `AppState`, and restore it only from
the frame-loop owner on retryable failure.

**Tech stack:** Rust, crossbeam channels, tui-textarea, Cargo tests, Node
contract validators.

## Task 1: Spec Gate And RED Tests

- [x] Audit response routing, optimistic cleanup, dispatcher error paths,
  shutdown/backpressure, and public/protocol contracts.
- [x] Write the proposed spec before production edits.
- [x] Add focused tests for failed response restoration and stale response
  retirement.
- [x] Run the new tests RED against the current implementation.

## Task 2: Implement Acknowledged Response Ownership

- [x] Add the bounded non-blocking crate-private response acknowledgement
  type/channel.
- [x] Capture and key-match the pending interaction composer snapshot.
- [x] Restore retryable failures and preserve stale/committed semantics.
- [x] Retire snapshots on newer interactions, terminal completion, and reset.
- [x] Run focused interaction, dispatcher, and canonical runtime tests.

## Task 3: Documentation, Review, And Gates

- [x] Update roadmap, implemented spec, source counts, and any required
  contract evidence.
- [x] Run compiler check, full serial TUI, PTY, runtime/Windows validators and
  self-tests, formatter, and diff checks.
- [x] Request independent review and resolve every Critical/Important finding.
- [x] Commit once as `fix(tui): acknowledge interaction responses`.
- [x] Rebase onto latest local `main` and repeat affected gates.
- [x] Fast-forward local `main`, repeat root gates, promptly remove only this
  worktree/branch, and verify unrelated worktrees remain.
