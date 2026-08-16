# TUI Hosted Operation Recovery Ownership

Status: Implemented on `codex/tui-hosted-operation-recovery`

## Context

At audited base `bb25b4b30`, the runtime surface already owns recovery
admission and durable operation state. The hosted TUI controller still owns the
small `ResumeOperation` and `CancelOperation` transaction policy inline in
`app.rs`:

- rejecting either action when no hosted thread exists;
- creating `TuiSurfaceActions` for the active thread;
- invoking the exact operation-id recovery command with the existing surface
  task control and event sender;
- adding the existing resume/cancel failure prefix.

These actions are one recovery-control family. Existing status/slash tests
cover action selection, and runtime tests cover the typed recovery semantics,
but no test calls a focused hosted recovery action owner.

## Decision

Add a private `hosted_operation.rs` module with a crate-private
`HostedOperationAction` enum and `handle_hosted_operation_action` entry point.
Move only `ResumeOperation` and `CancelOperation` transaction shaping there.
`app.rs` retains action selection, interrupt/background no-op handling,
action-channel lifecycle, and final controller shutdown.

The process-local command enum is not a second source of operation truth.
`TuiSurfaceActions::resume_operation` and `cancel_operation` remain the only
mutation authorities.

## Frozen Recovery Behavior

### Common no-thread path

- Resume and Cancel both emit exactly
  `OperationRejected("no recoverable operation is available")`.
- They perform no runtime mutation and publish no success event.

### Resume

- With an active thread, invoke typed `resume_operation` with the exact
  `SurfaceOperationId`, existing `TuiSurfaceTaskControl`, and event sender.
- Immediate failure emits
  `OperationRejected("failed to resume operation: <error>")`.
- Successful lifecycle/projection/terminal events remain runtime-owned; the
  hosted owner fabricates no success event.

### Cancel

- With an active thread, invoke typed `cancel_operation` with the exact
  `SurfaceOperationId`, existing `TuiSurfaceTaskControl`, and event sender.
- Immediate failure emits
  `OperationRejected("failed to cancel operation: <error>")`.
- Idempotence, terminal settlement, waiter behavior, and lifecycle/projection
  events remain runtime-owned; the hosted owner fabricates no success event.

## Failure, Cancellation, And Compatibility

- Recovery eligibility, stale fences, cancellation, terminal waits, retry,
  timeout, disconnect, and restart semantics remain runtime-owned and
  unchanged. This slice adds no retry, timeout, worker, or cancellation state.
- Action-channel disconnect still exits the controller and settles hosted
  actors. A failed recovery command leaves the active thread installed.
- No CLI/slash syntax, `UserAction`, `TuiEvent`, runtime surface, server/JSONL,
  app-server, ACP, transcript, session, operation schema, or persistence change.

## Test Strategy

1. Add a direct owner test through the absent module that calls Resume then
   Cancel without a thread and proves the exact two rejections, order, and
   absence of extra events.
2. Keep status-key/slash dispatch and typed recovery tests as behavioral
   evidence for operation selection and runtime mutation semantics.
3. Add path-specific controller/owner anchors for both actions, with negative
   validator self-tests that cannot pass from imports, enum variants, or
   test-only references.
4. Run focused owner/recovery/status tests, compiler check, full serial TUI,
   PTY, runtime/Windows validators and self-tests, formatter, and diff checks.
5. Request independent review focused on operation identity, no-thread/error
   shaping, runtime control ownership, validator integrity, and compatibility.

## Acceptance Criteria

1. ResumeOperation and CancelOperation have one transaction owner in
   `hosted_operation.rs`; `app.rs` only maps commands.
2. The direct owner test is RED before the API exists and GREEN afterward.
3. Existing recovery/status/slash and PTY behavior passes unchanged.
4. Contract validation rejects deletion of either production owner branch or
   controller dispatch while other textual references remain.
5. Full TUI and PTY suites pass after rebase and again on integrated local
   `main`.
6. Independent review has no unresolved Critical or Important finding.

## Implementation Evidence

- At audited base `bb25b4b30`, the direct owner test failed because
  `crate::hosted_operation` did not exist. The focused owner test passes after
  the module and handler were added.
- `hosted_operation.rs` owns common no-thread rejection plus the exact Resume
  and Cancel typed calls and error prefixes. The controller branches contain
  only crate-private command mapping.
- The direct owner test proves both exact no-thread rejections in action order
  and the absence of extra events. A second owner test uses a real active
  runtime thread with no recoverable operation to prove the exact Resume and
  Cancel typed-error prefixes and absence of fabricated success. Existing
  recovery status-key and cancel slash-command tests pass unchanged.
- Path-specific controller and owner anchors now cover both actions, and
  negative validator self-tests reject deletion while imports, enum variants,
  and test references remain. Independent review found no Critical or Important
  issue, and CodeRabbit reported no tracked-diff finding. Post-extraction source
  sizes are `app.rs` 8,823 lines and `hosted_operation.rs` 132 lines.

## Residual Boundary

Plan implementation, task stop/foreground, and background interaction control
remain controller-owned action families. Cold legacy registry reconciliation
remains an independent migration boundary.
