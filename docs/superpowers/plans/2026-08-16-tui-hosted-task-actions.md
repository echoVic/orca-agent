# TUI Hosted Task Action Ownership Plan

**Goal:** Give stop, foreground, and background-approval actions one hosted
owner while repairing prearmed activation rollback on non-foreground paths.

**Architecture:** Extend `background_tasks.rs` with a crate-private typed action
and handler, move the background-approval adapter into that owner, and reduce
the controller to command mapping. Runtime surface APIs retain mutation,
fencing, retry, timeout, and operation lifecycle authority.

**Tech stack:** Rust, crossbeam channels, runtime surface actions, Cargo tests,
Node contract validators.

## Task 1: Spec Gate And RED Lifecycle Test

- [x] Audit controller branches, dispatcher prearming, lower guards, task and
  interaction helpers, failure paths, tests, manifest, and source counts.
- [x] Write the proposed spec before production edits.
- [x] Add the direct missing-thread activation rollback test through the absent
  owner API.
- [x] Run it RED because the typed owner API does not exist.

## Task 2: Consolidate Hosted Task Actions

- [x] Add `HostedTaskAction` and `handle_hosted_task_action` to
  `background_tasks.rs`.
- [x] Move the background approval adapter into that owner and delete the
  redundant module.
- [x] Preserve stop/foreground behavior and repair activation rollback for
  every non-foreground approval path.
- [x] Reduce all three controller branches to typed command mapping.
- [x] Run direct owner and focused task/approval tests GREEN.

## Task 3: Documentation, Review, And Gates

- [x] Update roadmap, implemented spec, counts, manifest/digest references, and
  deletion-resistant validator self-tests.
- [x] Run compiler check, full serial TUI, PTY, runtime/Windows validators and
  self-tests, formatter, and diff checks.
- [x] Request independent review and resolve every Critical/Important finding.
- [x] Commit once as `fix(tui): own hosted task actions`.
- [x] Rebase onto latest local `main` and repeat affected gates.
- [x] Fast-forward local `main`, repeat root gates, and remove only this
  worktree/branch.
