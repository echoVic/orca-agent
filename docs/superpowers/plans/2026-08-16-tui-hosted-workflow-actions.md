# TUI Hosted Workflow Action Ownership Plan

**Goal:** Give saved-workflow launch one hosted transaction owner while
preserving typed runtime authority and exact event/error ordering.

**Architecture:** Add private `hosted_workflow.rs` with a crate-private
`HostedWorkflowAction` and `handle_hosted_workflow_action`. The controller maps
the existing `RunWorkflow` action into that owner; workflow admission and task
execution remain behind `TuiSurfaceActions`.

**Tech stack:** Rust, crossbeam channels, runtime surface actions, Cargo tests,
Node contract validators.

## Task 1: Spec Gate And RED Owner Test

- [x] Audit controller branch, runtime owner, success/failure ordering,
  behavioral tests, manifest rows, module dependencies, and source counts.
- [x] Write the proposed spec before production edits.
- [x] Add a direct owner test through the absent owner API.
- [x] Run it RED because `HostedWorkflowAction` and the handler do not exist.

## Task 2: Extract The Hosted Workflow Transaction

- [x] Add the private module, enum, handler, and owner test.
- [x] Move `RunWorkflow` with unchanged startup, readiness, launch, event,
  error, and desktop-notification ordering.
- [x] Reduce the controller branch to typed command mapping and remove helpers
  made test-only by the move.
- [x] Run direct owner and focused workflow tests GREEN.

## Task 3: Documentation, Review, And Gates

- [x] Update roadmap, implemented spec, counts, manifest/digest references, and
  deletion-resistant validator self-tests.
- [x] Run compiler check, full serial TUI, PTY, runtime/Windows validators and
  self-tests, formatter, and diff checks.
- [x] Request independent review and resolve every Critical/Important finding.
- [x] Commit once as `refactor(tui): own hosted workflow actions`.
- [x] Rebase onto latest local `main` and repeat affected gates.
- [x] Fast-forward local `main`, repeat root gates, and remove only this
  worktree/branch.
