# TUI Hosted Operation Recovery Ownership Plan

**Goal:** Give ResumeOperation and CancelOperation one hosted transaction owner
while preserving exact runtime recovery authority and error shaping.

**Architecture:** Add private `hosted_operation.rs` with a crate-private
`HostedOperationAction` and `handle_hosted_operation_action`. The controller
maps the two existing recovery actions into that owner; recovery admission,
operation lifecycle, and terminal settlement remain behind
`TuiSurfaceActions`.

**Tech stack:** Rust, crossbeam channels, runtime surface actions, Cargo tests,
Node contract validators.

## Task 1: Spec Gate And RED Owner Test

- [x] Audit controller branches, runtime owners, failure/lifecycle ordering,
  behavioral tests, manifest rows, module dependencies, and source counts.
- [x] Write the proposed spec before production edits.
- [x] Add a direct no-thread test through the absent owner API.
- [x] Run it RED because `HostedOperationAction` and the handler do not exist.

## Task 2: Extract The Hosted Recovery Transaction

- [x] Add the private module, enum, handler, and owner test.
- [x] Move ResumeOperation/CancelOperation with unchanged operation identity,
  no-thread, runtime call, error, and event ordering.
- [x] Reduce controller branches to typed command mapping.
- [x] Run direct owner and focused recovery/status tests GREEN.

## Task 3: Documentation, Review, And Gates

- [x] Update roadmap, implemented spec, counts, manifest/digest references, and
  deletion-resistant validator self-tests.
- [x] Run compiler check, full serial TUI, PTY, runtime/Windows validators and
  self-tests, formatter, and diff checks.
- [x] Request independent review and resolve every Critical/Important finding.
- [x] Commit once as `refactor(tui): own hosted operation recovery`.
- [x] Rebase onto latest local `main` and repeat affected gates.
- [x] Fast-forward local `main`, repeat root gates, and remove only this
  worktree/branch.
