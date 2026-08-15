# TUI Convergence Slice 23: Hosted Session Projection Ownership

## Status

Implemented on `codex/tui-hosted-session-projection`, based on clean local
`main` at `acae2d04a`. This is an internal TUI adapter extraction. It does not
change the runtime surface schema, persisted history, CLI, server/JSONL, ACP,
or user-visible session behavior.

## Problem And Evidence

The hosted Goal/turn adapter now has a module owner, but `app.rs` still mixes
stateless session projection and history-event shaping into the controller.
`announce_runtime_ready`, `read_hosted_projection_batch`,
`project_hosted_thread_attached`, `emit_typed_history_snapshot`, saved-history
fallback, startup eligibility, UUID recognition, and empty-history emission
are called from startup, Side reentry, saved-session switching, and submitted
turn paths. These helpers read a typed runtime snapshot and translate it into
existing TUI events; they do not own the runtime thread or session replacement
transaction.

This is an architecture-boundary issue, not a user-visible behavior fix. The
pre-extraction `app.rs` was 10,348 lines; after extraction, `app.rs` is 10,120
lines and `hosted_session.rs` is 341 lines. The repeated
projection/history adapter is now a stateless module below the
controller-owned session installation and shutdown transaction.

## Scope

Move these stateless helpers into `hosted_session.rs`:

- typed snapshot to `SurfaceProjectionState` and history-message conversion;
- attached reset/history publication;
- runtime-ready snapshot publication;
- saved-history fallback, startup eligibility, UUID recognition, and empty
  history publication.

Keep `app.rs` responsible for `RuntimeThreadHandle` installation, replacement,
shutdown/reaping, config/preloaded-session updates, session switching, and the
hosted controller loop. The new module receives all handles, channels, and
presentation inputs explicitly and remains crate-private.

## Non-Goals

- Do not move `ensure_hosted_thread`, session start/fork/resume/switch, or
  latest-active Goal recovery.
- Do not change attachment generations, reset/history ordering, projection
  cursor admission, cancellation, joins, notices, or terminal statuses.
- Do not change runtime surface, persistence, protocol, or public APIs.
- Do not add a second history/session store or compatibility wrapper.

## Ownership And Semantics

`hosted_session` owns only event shaping and pure eligibility decisions. The
controller retains ownership of the live thread, preloaded transcript, config,
attachment routing, and replacement transaction.

- Normal startup and switching publish the same `HistoryLoaded` and
  `SurfaceProjectionSynced` payloads in the same order.
- Side hydration publishes attached reset then inherited history through the
  same attachment sender and preserves generation fencing.
- Cancellation and shutdown remain in the controller and typed surface; the
  module starts no workers and retains no cancellation state.
- History fallback and startup failures preserve the existing error strings and
  empty-history behavior.
- Timeout, retry, disconnect, and restart policy remain in the runtime surface
  and controller; this slice adds no policy.

## Acceptance

1. The listed projection/history helpers have one module owner and no duplicate
   implementation remains in `app.rs`.
2. Existing startup, saved-session, Side, Goal, ordinary-turn, task-control,
   restart, and PTY behavior remains green.
3. Focused module tests cover UUID/startup eligibility, empty-history emission,
   and attached reset/history ordering; existing integration tests cover real
   snapshot hydration and session switching.
4. Full TUI, PTY, validators, formatter, and diff checks pass.
5. Independent review finds no changed event order, attachment fencing,
   session ownership, persistence, or public API behavior.

## Evidence

- `hosted_session` focused tests: 3 passed.
- The exact Side reentry test passed: 1 passed.
- Full `orca-tui` library suite: 1,074 passed.
- Root PTY contract: 6 passed.
- `cargo check -p orca-tui --tests --locked`: passed.
- Runtime-surface and Windows-boundary validators passed; formatter and diff
  checks passed. These gates are repeated after rebasing onto local `main`.

## Verification Commands

```bash
cargo test -p orca-tui hosted_session --lib --locked -- --test-threads=1
cargo test -p orca-tui app::tests::hosted_side_switches_project_recorded_parent_and_ephemeral_side_identity --lib --locked -- --exact --test-threads=1
cargo test -p orca-tui --lib --locked -- --test-threads=1
cargo test --test tui_pty_contract --locked -- --test-threads=1
node scripts/validate-runtime-surface-contract.mjs
node scripts/validate-windows-platform-boundaries.mjs
cargo fmt --all -- --check
git diff --check
```

## Migration And Rollback

Use one isolated worktree and one semantic commit. Reverting the commit
restores the helper locations without data migration or protocol work.

## Self-Review

The boundary stops before session replacement and shutdown ownership. A later
session-lifecycle slice may move those transactions only with dedicated
recovery and reaping evidence.
