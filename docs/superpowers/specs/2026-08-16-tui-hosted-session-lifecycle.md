# TUI Convergence Slice 24: Hosted Session Lifecycle Ownership

## Status

Implemented on `codex/tui-hosted-session-lifecycle`, based on clean local
`main` at `514a14a11`. This slice moves the existing hosted-session lifecycle
transaction helpers behind one crate-private owner. It does not change the
runtime surface schema, persisted history, CLI, server/JSONL, ACP, or
user-visible session behavior.

## Problem And Evidence

The hosted snapshot/history adapters now have an owner, but `app.rs` still
contains the session lifecycle transaction: starting a missing thread,
checking whether the current session can be replaced, preflighting a newly
started thread against its typed snapshot, installing/replacing the active
thread, asynchronously reaping the retired thread, starting new/forked
sessions, switching saved sessions, and refreshing the saved-session list.
These helpers are called from the controller, Side startup, picker actions,
and submitted-turn paths. Keeping them in the event loop leaves thread
creation, preflight, replacement, and reaping without a single named owner.

This is an architecture and lifecycle ownership issue, not a user-visible
behavior fix. The pre-extraction `app.rs` was 10,120 lines; after extraction,
`app.rs` is 9,842 lines and `hosted_session_lifecycle.rs` is 309 lines. The
existing tests already prove session preflight failure preserves the previous
runtime, new/fork/resume flows retain their transcripts, Side startup validates
identity, and picker actions refresh their saved-session list.

## Scope

Move these helpers into `hosted_session_lifecycle.rs`:

- `ensure_hosted_thread`;
- `ensure_current_session_switchable`;
- `preflight_started_session`;
- `install_hosted_session`;
- `reap_hosted_thread`;
- `start_new_hosted_session`;
- `start_forked_hosted_session`;
- `switch_saved_hosted_session`; and
- `refresh_saved_session_picker`.

Keep the controller loop, attachment routing/rotation, Side lifecycle, latest
active Goal recovery, submitted-turn dispatch, config/preloaded handles, and
final shutdown policy in `app.rs`. The lifecycle module receives all mutable
handles and channels explicitly; it owns no global state and introduces no
worker beyond the existing bounded session reaper.

## Non-Goals

- Do not change runtime thread startup, snapshot preflight, replacement,
  reaping deadlines, error text, attachment generations, or event order.
- Do not move latest-active Goal recovery or the hosted controller loop.
- Do not change cancellation, timeout, retry, disconnect, or restart policy.
- Do not change runtime surface, persistence, protocol, or public APIs.
- Do not add a compatibility wrapper or a second lifecycle state source.

## Ownership And Semantics

`hosted_session_lifecycle` owns the existing start/preflight/install/reap
transaction helpers. The controller retains when to invoke them, owns the
active attachment and event loop, and remains responsible for latest-active
Goal recovery and final shutdown.

- Missing-thread startup still consumes and clears the test-only preloaded
  transcript fixture only after a successful runtime start, and emits the same
  recovered-approval notices in every build.
- New, fork, resume, and Side starts still preflight snapshot identity before
  installation; failed preflight reaps the candidate and leaves the old thread
  untouched.
- Successful installation still replaces the active thread, clears preloaded
  and pending workflow state, and reaps the retired thread asynchronously.
- Session replacement still rejects active foreground/queued/background
  operations, non-terminal tasks/workflows, and active Goals with the same
  message.
- Cancellation, timeout, retry, disconnect, and restart behavior remains in
  the runtime surface and controller; this slice adds no policy.

## Acceptance

1. All listed lifecycle helpers have one module owner and no duplicate body
   remains in `app.rs`.
2. Existing startup, Side, new, fork, resume, picker, preflight-failure,
   restart, Goal, ordinary-turn, and PTY behavior remains green.
3. Focused lifecycle tests cover the no-current-session switch decision and
   existing integration tests cover preflight, replacement, reaping, and
   saved-session behavior.
4. Full TUI, PTY, validators, formatter, and diff checks pass.
5. Independent review finds no changed ownership, lifecycle, attachment,
   persistence, event ordering, or public API behavior.

## Evidence

- Lifecycle unit test: 1 passed.
- Preflight, fork, new-session, picker, Side, and restart-focused tests passed.
- Full serial TUI library suite: 1,075 passed.
- Root PTY contract suite: 6 passed.
- Runtime-surface validator and self-tests passed after moving the reviewed
  lifecycle call sites to their new source path; the protocol manifest did not
  change.
- Windows boundary validator and self-tests, `cargo check`, formatter, and diff
  checks passed.
- Independent review found no Critical or Important findings; its preload
  wording note is corrected above.
- Affected gates are repeated after rebasing onto local `main`.

## Verification Commands

```bash
cargo test -p orca-tui hosted_session_lifecycle --lib --locked -- --test-threads=1
cargo test -p orca-tui app::tests::session_preflight_failure_preserves_previous_runtime_and_projection --lib --locked -- --exact --test-threads=1
cargo test -p orca-tui app::tests::hosted_tui_fork_preserves_source_and_projects_copied_history --lib --locked -- --exact --test-threads=1
cargo test -p orca-tui --lib --locked -- --test-threads=1
cargo test --test tui_pty_contract --locked -- --test-threads=1
node scripts/validate-runtime-surface-contract.mjs
node scripts/validate-windows-platform-boundaries.mjs
cargo fmt --all -- --check
git diff --check
```

## Migration And Rollback

Use one isolated worktree and one semantic commit. Reverting the commit
restores the lifecycle helper locations without data migration or protocol
work.

## Self-Review

The boundary stops before controller event routing and latest-active Goal
recovery. A later controller-loop slice must preserve attachment barriers and
recovery ownership with separate lifecycle evidence.
