# TUI Convergence Slice 27: Hosted Latest-Active Goal Recovery Ownership

## Status

Implemented on `codex/tui-hosted-goal-recovery`, based on clean local `main`
at `c1483900c`. This slice moves the existing latest-active Goal recovery
transaction from `app.rs` into `hosted_session_lifecycle.rs`. It changes no
runtime surface schema, persisted history, CLI, server/JSONL, ACP, or
user-visible Goal behavior.

## Problem And Evidence

The hosted Goal and session slices already give stateless Goal dispatch and
session replacement an owner, but `app.rs` still owns
`resume_latest_active_goal_hosted`. That path discovers the latest active
persisted Goal, loads its transcript, starts a candidate runtime thread,
revalidates the Goal through the candidate typed surface, retires the old
thread, updates the shared history mode, clears the preloaded-session
boundary, refreshes recovered approvals, and starts the continuation. It is a
session replacement transaction, but remains embedded beside the controller's
action loop and final shutdown. Before extraction, `app.rs` was 9,613 lines
and `hosted_session_lifecycle.rs` was 309 lines; after extraction they are
9,518 and 447 lines respectively.

This is an architecture and lifecycle ownership issue, not a behavior fix.
The candidate must be fully loaded and surface-validated before the old
runtime is touched; failures must preserve the old thread, config, and
preloaded state. The existing integration tests already exercise successful
latest-active recovery, no-active-Goal handling, persistence-source selection,
and continuation interruption.

## Scope

Move `resume_latest_active_goal_hosted` into
`hosted_session_lifecycle.rs` and expose it as a crate-private lifecycle API.
Update only the controller call site and imports. Keep
`send_hosted_action_failure`, the controller action loop, attachment routing,
Side lifecycle, queued scheduling, and final shutdown in `app.rs`.

## Non-Goals

- Do not change Goal lookup, transcript loading, runtime-start, surface
  validation, replacement, approval-notice, continuation, or error ordering.
- Do not change cancellation, timeout, retry, disconnect, or restart policy.
- Do not add a worker, cache, compatibility wrapper, or second Goal/session
  fact source.
- Do not change runtime surface, persistence, protocol, public APIs, or Goal
  continuation semantics.
- Do not extract the controller loop or Side attachment lifecycle.

## Ownership And Semantics

`hosted_session_lifecycle` owns session replacement transactions, including
latest-active Goal recovery. The controller still decides when `/goal resume`
is requested, owns the active attachment/event loop, and performs final
shutdown.

- Disabled history emits the existing persistent-history error and leaves all
  state unchanged.
- No active Goal emits `GoalStatus(None)`; Goal-store, transcript-load, and
  runtime-start failures emit their existing errors without replacing the
  current thread.
- A candidate whose typed surface has no matching Goal is shut down and the
  current thread/config/preloaded state remain intact.
- Only after candidate validation does recovery retire the old thread, install
  the candidate, clear preloaded state, update `HistoryMode::Resume`, notify
  recovered approvals, and start the existing continuation.
- Continuation cancellation, terminal recovery errors, and ordinary operation
  errors retain the existing `TuiSurfaceActions` and event semantics.

## Acceptance

1. The recovery helper has one owner in `hosted_session_lifecycle.rs`; no
   duplicate body remains in `app.rs`.
2. A focused lifecycle test covers the disabled-history rejection without
   starting a runtime thread; existing integration tests cover successful
   recovery, no-goal, persistence-source selection, interruption, and restart
   behavior.
3. Candidate validation precedes old-thread replacement, and all state/error
   ordering remains behaviorally unchanged.
4. Full serial TUI, root PTY, runtime/Windows validators, formatting, and diff
   checks pass.
5. Independent review finds no changed lifecycle, cancellation, attachment,
   persistence, event ordering, or public API behavior.

## Evidence

- RED compilation failed because `hosted_session_lifecycle` did not yet expose
  `resume_latest_active_goal_hosted`; the old private `app.rs` body was not
  accessible to the new module test.
- GREEN lifecycle tests passed 2/2, including disabled-history rejection with
  no thread or preload mutation.
- Exact successful latest-active recovery, no-active-Goal, and legacy-temp
  persistence-source tests passed.
- `cargo check -p orca-tui --tests --locked` passed.
- Full serial TUI library suite passed 1,083/1,083 and the root PTY contract
  passed 6/6.
- The runtime-surface validator passed after relocating the same two shutdown
  calls, one runtime-start call, and reviewed Goal callback manifest anchor to
  the lifecycle owner. No runtime protocol inventory changed.
- Runtime-surface and Windows validator self-tests, direct Windows validation,
  formatter, ownership search, manifest digest comparison, and diff integrity
  checks passed.
- Independent review found one Important validator-integrity issue: a bare
  callback-name anchor could pass on an import after the production call was
  removed. The anchor is now call-shaped, the manifest names the definition
  and call sites, and a negative mutation self-test preserves the import while
  requiring deletion of the call to fail validation. Re-review found no
  remaining Critical or Important findings.
- Goal-store read/load/start and candidate-snapshot failure branches remain
  byte-equivalent apart from module qualification but are not individually
  fault-injected; this is a residual non-blocking test gap for a relocation.

## Verification Commands

```bash
cargo test -p orca-tui hosted_session_lifecycle --lib --locked -- --test-threads=1
cargo test -p orca-tui app::tests::empty_recorded_hosted_tui_goal_resume_restores_latest_active_goal --lib --locked -- --exact --test-threads=1
cargo test -p orca-tui app::tests::empty_recorded_hosted_tui_goal_resume_without_active_goal_reports_none --lib --locked -- --exact --test-threads=1
cargo test -p orca-tui app::tests::goal_resume_ignores_legacy_json_temp_directory --lib --locked -- --exact --test-threads=1
cargo test -p orca-tui --lib --locked -- --test-threads=1
cargo test --test tui_pty_contract --locked -- --test-threads=1
node scripts/validate-runtime-surface-contract.mjs
node scripts/test-validate-runtime-surface-contract.mjs
node scripts/validate-windows-platform-boundaries.mjs
node scripts/test-validate-windows-platform-boundaries.mjs
cargo fmt --all -- --check
git diff --check
```

## Migration And Rollback

Use one isolated worktree and one semantic commit. Reverting the commit
restores the recovery helper to `app.rs` without data migration or protocol
work.

## Self-Review

The transaction boundary is complete because candidate startup, typed Goal
validation, replacement, preload/config mutation, approval refresh, and
continuation launch remain one ordered operation. A later controller-loop
slice must preserve this lifecycle API and the attachment barriers.
