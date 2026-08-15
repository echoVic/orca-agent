# TUI Convergence Slice 26: Hosted Submission Ownership

## Status

Implemented on `codex/tui-hosted-submission`, based on clean local `main` at
`77893f42c`. This slice moves the existing submitted-turn transaction behind
one crate-private owner. It does not change the runtime surface schema,
persisted history, CLI, server/JSONL, ACP, or user-visible submission behavior.

## Problem And Evidence

`hosted_runtime` owns stateless request construction and ordinary-turn
dispatch, `hosted_goal` owns Goal-run dispatch, and
`hosted_session_lifecycle` owns thread startup. The transaction that composes
those owners still lives in `app.rs`: capture queued rejection identity, clone
the active configuration, start a missing thread, announce readiness, expand
bound mentions against the runtime workspace roots, choose ordinary versus
Goal dispatch from durable session identity, and emit the existing completion
notification.

That transaction is called by direct, mention-aware, queued, workflow
notification, plan-implementation, and Side submissions. Keeping it in the
controller file leaves submission ordering and failure shaping without one
named owner. The pre-extraction `app.rs` is 9,674 lines. Existing behavior tests
already cover failed startup, queued preflight failure, typed ordinary-turn
dispatch, workflow notification title/label semantics, Side submission, Goal
dispatch, terminal recovery error shaping, and PTY submission. After
extraction, `app.rs` is 9,613 lines and `hosted_submission.rs` is 254 lines.

## Scope

Move `handle_hosted_submitted_turn` into `hosted_submission.rs`. The module
receives the submitted turn, active config, preload handle, mutable runtime
thread slot, event sender, task control, pending workflow-notification handle,
and runtime host explicitly. It composes existing owners without introducing
global state or a worker.

Keep action selection, queued-input scheduling, Side selection, plan gating,
attachment routing, latest-active Goal recovery, final controller shutdown,
and runtime thread replacement policy in `app.rs`.

## Non-Goals

- Do not change prompt/title selection, mention expansion roots, patching of
  bound mentions, queued id preservation, or rejection text.
- Do not change when a missing thread starts or when `MentionRuntimeReady` and
  the surface projection are emitted.
- Do not change ordinary-versus-Goal dispatch, task labels, backtrack flags,
  desktop notifications, or terminal event shaping.
- Do not change cancellation, timeout, retry, disconnect, restart, or latest
  active Goal recovery policy.
- Do not change runtime surface, persistence, protocol, or public APIs.

## Ownership And Semantics

`hosted_submission` owns the existing submitted-turn transaction.

- It snapshots config before startup and uses the same configured cwd and
  non-empty runtime workspace roots, falling back to cwd exactly as before.
- A failed thread start publishes `SubmissionRejected` for user/queued input
  with the original visible prompt and queued id; workflow notifications retain
  the existing generic error path. No preload or active-thread state is
  fabricated on failure.
- A newly started thread publishes `MentionRuntimeReady` and its surface
  projection before mention expansion and turn dispatch, preserving the
  existing order.
- Mention expansion completes before request construction. Failure uses the
  same submission-error shaping and does not start a turn.
- Sessionless threads use ordinary typed-surface turn dispatch. Recorded
  threads continue through the Goal-aware dispatch owner; that owner decides
  whether an active Goal exists.
- The existing desktop completion notification remains after recorded/Goal
  dispatch only. Runtime cancellation, terminal recovery, retry, disconnect,
  and restart behavior remains delegated to existing owners.

## Acceptance

1. `handle_hosted_submitted_turn` has one module owner and no duplicate body
   remains in `app.rs`.
2. Focused behavior tests prove startup rejection preserves the original prompt
   and preloaded state, while queued mention rejection preserves queued identity
   and does not fabricate a successful submission.
3. Existing direct, mention-aware, queued, workflow notification, plan, Side,
   ordinary-turn, Goal, terminal-recovery, and restart tests remain green.
4. Full TUI, PTY, validators, compile check, formatter, and diff checks pass.
5. Independent review finds no changed startup, prompt, event-order, runtime
   authority, cancellation, recovery, persistence, or public-API behavior.

## Evidence

- RED: both owner tests failed only because
  `hosted_submission::handle_hosted_submitted_turn` did not exist while the
  inaccessible legacy body remained in `app.rs`.
- GREEN: two focused owner tests pass for startup rejection/preload retention
  and queued stale-mention rejection after readiness publication. The existing
  startup regression also passes through the new owner.
- Exact queued stale-mention, workflow-notification title, and terminal-recovery
  regressions pass; the Goal filter passes all 38 tests.
- Full serial TUI suite: 1,082 passed on the current tree.
- `cargo check -p orca-tui --tests --locked` passes.
- Root PTY suite: 6 passed. Runtime-surface and Windows boundary validators
  plus their self-tests passed.
- Formatter and diff checks pass.
- Independent review found no Critical or Important correctness, lifecycle,
  ownership, compatibility, or test-evidence findings. Direct desktop
  notification coverage remains a non-blocking gap; its body and order are
  unchanged.
- Rebase and integrated-root repetitions remain closing gates.

## Verification Commands

```bash
cargo test -p orca-tui hosted_submission --lib --locked -- --test-threads=1
cargo test -p orca-tui app::tests::workflow_notification_first_turn_uses_notification_label_for_session_title --lib --locked -- --exact --test-threads=1
cargo test -p orca-tui app::tests::terminal_recovery_error_does_not_fabricate_failure_terminal --lib --locked -- --exact --test-threads=1
cargo test -p orca-tui goal_ --lib --locked -- --test-threads=1
cargo test -p orca-tui --lib --locked -- --test-threads=1
cargo test --test tui_pty_contract --locked -- --test-threads=1
node scripts/validate-runtime-surface-contract.mjs
node scripts/test-validate-runtime-surface-contract.mjs
node scripts/validate-windows-platform-boundaries.mjs
node scripts/test-validate-windows-platform-boundaries.mjs
cargo check -p orca-tui --tests --locked
cargo fmt --all -- --check
git diff --check
```

## Migration And Rollback

Use one isolated worktree and one semantic commit. Reverting the commit restores
the helper location without data migration or protocol work.

## Self-Review

The boundary deliberately stops before latest-active Goal recovery. Recovery
loads durable state, starts and installs a replacement thread, updates shared
configuration, clears preload state, reaps the previous thread, and starts Goal
execution as one lifecycle transaction. It requires a separate slice and
failure matrix.
