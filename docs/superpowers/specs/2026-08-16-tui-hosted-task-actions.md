# TUI Hosted Task Action Ownership

Status: Implemented on `codex/tui-hosted-task-actions`

## Context

At audited base `c2b89f47f`, the hosted TUI controller still constructs
`TuiSurfaceActions` inline for three related user actions:

- `StopTask` delegates to `background_tasks::stop_task_for_tui`;
- `ForegroundTask` delegates to `background_tasks::foreground_task_for_tui`;
- `ResolveBackgroundApproval` delegates to
  `background_approval::submit_background_approval_response_for_tui`.

The runtime surface already owns task fencing, interaction routing, operation
foregrounding, cancellation, retries, and durable projection. The controller
only chooses the current optional thread and shapes the hosted action.

There is also one lifecycle defect. `action_dispatcher` prearms surface
activation for every `ResolveBackgroundApproval`. The approved path consumes
that activation only after it finds the durable interaction and owning task.
The denied path never enters `SurfaceActivationGuard`, and missing-thread or
pre-guard failures return without cancelling the prearmed activation. The TUI
can then accept an idle interrupt as if an operation were starting and carry
that interrupt into the next operation.

## Decision

Make `background_tasks.rs` the single hosted task-action owner. Add a
crate-private `HostedTaskAction` and `handle_hosted_task_action` entry point for
stop, foreground, and background approval resolution. Move the small
background-approval response adapter into this module and remove the redundant
`background_approval.rs` module. `app.rs` retains `UserAction` selection and
maps each existing action to the crate-private command.

This owner is not a second task reducer or interaction broker. It constructs
the current thread-scoped `TuiSurfaceActions` facade and delegates all mutation
to the existing runtime-surface APIs.

## Frozen Action Semantics

### Stop

1. Preserve the exact task id.
2. Without a current thread, emit exactly
   `Error("cannot stop task before a session exists")` and return `false`.
3. On committed runtime success, emit `SurfaceProjectionSynced` before
   `Notice("Task stop requested for {task_id}.")` and return `true`.
4. On runtime failure, emit exactly one `Error` containing the existing error
   and return `false`.

### Foreground

1. Preserve the exact task id.
2. Without a current thread, emit exactly
   `Error("cannot foreground task before a session exists")` and return
   `false`.
3. On committed runtime success, preserve runtime-projected output and emit the
   returned `SurfaceProjectionSynced` before
   `Notice("Task {task_id} returned to foreground.")`.
4. On runtime failure, emit exactly one existing `Error` and return `false`.

### Resolve Background Approval

1. Preserve the exact approval id and allow/deny decision.
2. Without a current thread, emit exactly
   `Error("cannot resolve background approval before a session exists")`.
3. On committed response, emit `SurfaceProjectionSynced` before
   `Notice("Background approval {approved|denied} for {task_id}.")`.
4. On runtime failure, emit exactly one existing `Error`.
5. If the action does not install an approved foreground operation, cancel the
   dispatcher-prearmed surface activation before returning. This includes
   denial, missing thread, durable interaction/task lookup failure, and
   rejected/deferred/uncommitted response.
6. If approval installs a foreground operation, the existing
   `SurfaceActivationGuard` and `SurfaceRunGuard` remain responsible for
   consuming, cancelling, or settling the activation and operation.

## Lifecycle And Compatibility

- Task and interaction revisions, task/operation fences, exact response route,
  runtime retries, the 500 ms foreground lookup bound, the 5 second background
  approval lookup/response bound, disconnect errors, and terminal settlement
  remain runtime-surface owned and unchanged.
- Stop and foreground cancellation behavior remains in
  `TuiSurfaceTaskControl` and the runtime surface. This slice adds no worker,
  retry loop, timeout, task cache, cancellation token, or durable state.
- Restart recovery and recovered background-approval notification remain in
  hosted session lifecycle plus `notify_recovered_background_approvals_for_tui`.
- UI task selection and approval-dialog resolution remain in
  `workflow_panel_actions` and `approval_actions`.
- No `UserAction`, `TuiEvent`, runtime surface, CLI/slash syntax, server/JSONL,
  app-server, ACP, transcript, schema, persistence, or public API changes.

## Test Strategy

1. Add a direct owner test through the absent `HostedTaskAction` API. Prearm
   activation, resolve a background approval without a thread, and prove the
   exact error plus the ability to arm again. This is RED before the owner
   exists and catches the current lifecycle leak.
2. In the same owner suite, prove missing-thread stop and foreground preserve
   their exact errors and do not fabricate projections or notices.
3. Keep existing real-runtime stop, foreground, approval allow/deny,
   background reentry, interruption, and PTY tests as downstream evidence.
4. Add path-specific controller and owner anchors for all three actions, with
   negative validator self-tests that imports, enum variants, and UI emitters
   cannot satisfy.
5. Run focused owner/task/approval tests, compiler check, full serial TUI, PTY,
   runtime/Windows validators and self-tests, formatter, and diff checks.
6. Request independent review focused on activation rollback, exact event
   order, runtime ownership, validator integrity, and compatibility.

## Acceptance Criteria

1. The three hosted task actions have one transaction owner in
   `background_tasks.rs`; `app.rs` only maps existing user actions.
2. The activation-leak owner test is RED before the API/fix and GREEN after it.
3. Denial and every failure before foreground installation release the
   dispatcher-prearmed activation; approved foreground execution retains its
   existing lower-level guards.
4. Existing task stop/foreground and background approval allow/deny behavior
   passes unchanged apart from the lifecycle repair.
5. Contract validation rejects deletion of each production controller mapping
   or owner branch while other textual references remain.
6. Full TUI and PTY suites pass after rebase and again on integrated local
   `main`.
7. Independent review has no unresolved Critical or Important finding.

## Implementation Evidence

- At audited base `c2b89f47f`, the direct lifecycle test failed only because
  `HostedTaskAction` and `handle_hosted_task_action` did not exist. Both direct
  owner tests pass after the typed owner and rollback were added.
- `background_tasks.rs` now owns all three hosted action transactions. The
  controller contains only exact typed-command mappings, and the redundant
  `background_approval.rs` adapter has been removed.
- The missing-thread regression proves the exact error and released prearmed
  activation. The real-runtime denial regression proves a committed denial is
  followed by an idle interrupt and a successful next turn, so the denied
  action cannot carry cancellation into the next operation.
- Path-specific controller and owner anchors for all three actions, their
  negative validator self-tests, and the relocated mutation-site counts pass.
  Post-extraction source sizes are `app.rs` 8,826 lines and
  `background_tasks.rs` 239 lines.
- The initial full serial TUI suite passes 1,095/1,095 and the root-package PTY
  contract passes 6/6. Compiler check, runtime and Windows validators plus
  their self-tests, formatter, and diff checks also pass.
- Independent review found no Critical or Important correctness issue and
  confirmed the activation rollback, approved-success lower guards, exact
  relocated behavior, controller-only mapping, and deletion-resistant
  validator coverage. CodeRabbit reported no issue in the tracked diff.
- After the topic was confirmed up to date with local `main`, the full serial
  TUI suite and PTY contract passed again on the topic and on integrated local
  `main` with the same 1,095/1,095 and 6/6 results. All validator, formatter,
  and diff gates also passed in both locations.

## Residual Boundary

Foreground approval/permission/user-input/MCP interaction responses remain a
separate hosted interaction family. Cold legacy registry reconciliation remains
an independent migration boundary.
