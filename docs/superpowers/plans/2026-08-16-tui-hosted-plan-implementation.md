# TUI Hosted Plan Implementation Ownership Plan

**Goal:** Give approved-plan implementation one hosted transaction owner while
preserving settings-before-submit ordering and activation rollback.

**Architecture:** Add private `hosted_plan.rs` with a crate-private command and
handler. The controller maps `ImplementApprovedPlan` into that owner;
`hosted_settings` and `hosted_submission` retain their existing mutation and
lifecycle authority.

**Tech stack:** Rust, crossbeam channels, runtime surface actions, Cargo tests,
Node contract validators.

## Task 1: Spec Gate And RED Owner Test

- [x] Audit the controller branch, dispatcher activation, settings/submission
  owners, event ordering, failure paths, tests, manifest, and source counts.
- [x] Write the proposed spec before production edits.
- [x] Add the direct sessionless-rejection test through the absent owner API.
- [x] Run it RED because `HostedPlanAction` and the handler do not exist.

## Task 2: Extract The Hosted Plan Transaction

- [x] Add the private module, command, handler, and rejection/success owner tests.
- [x] Move approval patching, success event, submission delegation, and failure
  activation rollback without changing prompt, mode, or collaborator identity.
- [x] Reduce the controller branch to typed command mapping.
- [x] Run both direct owner and focused plan/settings/submission tests GREEN.

## Task 3: Documentation, Review, And Gates

- [x] Update roadmap, implemented spec, counts, manifest/digest references, and
  deletion-resistant validator self-tests.
- [x] Run compiler check, full serial TUI, PTY, runtime/Windows validators and
  self-tests, formatter, and diff checks.
- [x] Request independent review and resolve every Critical/Important finding.
- [x] Commit once as `refactor(tui): own hosted plan implementation`.
- [x] Rebase onto latest local `main` and repeat affected gates.
- [x] Fast-forward local `main`, repeat root gates, and remove only this
  worktree/branch.
