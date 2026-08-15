# TUI Convergence Slice 22: Hosted Goal Orchestration Ownership

## Status

Implemented on `codex/tui-goal-orchestration`, based on clean local `main` at
`112ced075`. This is an internal TUI orchestration extraction. It does not
change the runtime surface schema, persisted history, CLI, server/JSONL, ACP,
or user-visible Goal behavior.

## Problem And Evidence

The accepted runtime surface now owns Goal facts and the task projection, but
`crates/orca-tui/src/app.rs` still owned the Goal execution/recovery adapter in
the renderer module before this slice. `app.rs` was 10,559 lines at the
baseline; `run_hosted_goal_run`, Goal session-id resolution, `show_hosted_goal`,
latest-active Goal recovery, request construction, ordinary turn execution,
and operation-error shaping were mixed into that controller file and called
from both the hosted action loop and submitted turn path. This left
renderer-owned orchestration as the next TUI/runtime protocol boundary in the
roadmap.

The code already has a natural boundary: `TuiSurfaceActions` performs the
typed mutation and projection, while the adapter only chooses the Goal/turn
operation, translates outcomes into TUI events, and manages the existing
runtime-thread handles. The extraction can therefore preserve the existing
behavioral oracle without creating a second Goal store or changing runtime
ownership.

Classification: architecture boundary, not a user-visible behavior fix.

## Scope

Move the hosted Goal/turn orchestration helpers into focused modules:

- `hosted_runtime.rs` owns `hosted_turn_request`, ordinary typed-turn
  invocation, submission-rejection shaping, and shared
  operation-error/terminal-failure event shaping;
- `hosted_goal.rs` owns Goal continuation prompts, Goal session-id lookup,
  Goal display, Goal history errors, and Goal-run dispatch;
- `app.rs` retains latest-active Goal recovery because it installs and shuts
  down the controller-owned runtime thread and updates the preloaded-session
  boundary atomically;
- `app.rs` retains the action loop and session/Side lifecycle state, calling
  the module APIs with the same handles, configs, channels, and cancellation
  controller.

The module APIs remain crate-private. The runtime surface remains the only
source of Goal/task facts; `AppState` remains the presentation owner.

## Non-Goals

- Do not extract `hosted_tui_controller_loop` or Side/session attachment
  lifecycle in this slice.
- Do not extract latest-active Goal recovery; its runtime-thread installation,
  shutdown, preloaded-session clearing, and config update remain one controller
  transaction until a dedicated session-lifecycle slice can own that boundary.
- Do not change `TuiSurfaceActions`, runtime operations, cancellation, joins,
  notices, terminal statuses, or Goal persistence.
- Do not remove the source-compatible pending-interaction shim or reconcile
  legacy registry records.
- Do not add compatibility wrappers that leave a second long-lived Goal
  orchestration path in `app.rs`.

## Ownership And Semantics

`hosted_runtime` is a stateless adapter over `RuntimeThreadHandle` and
`TuiSurfaceActions`; it does not retain Goal state. `hosted_goal` receives all
state it needs from the controller and returns through existing TUI events and
typed operation outcomes. Latest-active recovery remains in `app.rs` because
the controller owns the runtime-thread installation and shutdown transaction.
Existing error, queued-submission, GoalStatus, SessionCompleted,
desktop-notification, and recovery semantics remain exact.

The extraction is a pure relocation plus module-visibility plumbing. A failed
runtime mutation still emits the same error and terminal event; a terminal
recovery error still suppresses the fabricated failure terminal; a missing or
disabled history session still produces the same Goal history error.

Lifecycle cases remain owned by their existing boundaries:

- Normal execution dispatches through the typed surface, preserving GoalStatus,
  queued-submission, assistant, and terminal event ordering.
- Cancellation remains controller-owned through `TuiSurfaceTaskControl`; the
  extracted adapters neither replace the token nor detach a worker, and the
  existing join path remains in the typed surface/controller.
- Rejection preserves `SubmissionRejected` for queued prompts and `Error` for
  unqueued submissions, including the original prompt and queue id.
- Timeout and retry behavior is unchanged because this slice adds no retry or
  deadline policy; the runtime surface and operation controller retain those
  decisions.
- Disconnect/error behavior continues through `emit_hosted_operation_error`:
  ordinary failures publish the failure terminal, while terminal-recovery
  errors publish only the recovery error and leave restart handling to `app.rs`.
- Restart and latest-active Goal recovery remain in `app.rs`, which installs
  the replacement runtime thread, loads the preloaded transcript, updates
  config, clears the preload, and shuts down the old thread as one transaction.

## Acceptance

1. Goal/turn orchestration helpers have one module owner and no duplicate
   implementation remains in `app.rs`.
2. Existing hosted Goal, ordinary-turn, Side, task-control, restart, and PTY
   behavior remains green; no runtime surface or persistence contract changes.
3. Focused unit coverage proves request construction and operation-error
   shaping remain unchanged, while existing integration tests cover Goal
   start/edit/clear/pause/resume and latest-active recovery through the
   controller-owned path.
4. `cargo test -p orca-tui --lib --locked -- --test-threads=1`, the root PTY
   contract, formatter, diff checks, and runtime-surface validators pass.
5. Independent review finds no lost cancellation, altered notice ordering,
   duplicate Goal source, or public API change.

Review result: the independent audit found no Critical or Important findings;
the only residual note was that the lifecycle matrix above should be explicit
for this pure relocation.

## Verification Commands

```bash
cargo test -p orca-tui hosted_runtime --lib --locked -- --test-threads=1
cargo test -p orca-tui goal_ --lib --locked -- --test-threads=1
cargo test -p orca-tui app::tests::hosted_side_background_task_foreground_uses_surface_projection --lib --locked -- --exact --test-threads=1
cargo test -p orca-tui --lib --locked -- --test-threads=1
cargo test --test tui_pty_contract --locked -- --test-threads=1
node scripts/validate-runtime-surface-contract.mjs
node scripts/validate-windows-platform-boundaries.mjs
cargo fmt --all -- --check
git diff --check
```

## Migration And Rollback

Use one isolated worktree and one semantic commit. Reverting the commit
restores the original helper locations without data migration or protocol
compatibility work.

## Self-Review

The boundary is intentionally limited to stateless hosted Goal/turn
orchestration. Session/Side lifecycle remains open for a later slice with its
own ownership and attachment-fencing evidence.
