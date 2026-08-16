# TUI Hosted Workflow Action Ownership

Status: Implemented on `codex/tui-hosted-workflow-actions`

## Context

At audited base `f9d872b29`, the runtime surface already owns saved-workflow
admission and durable workflow execution. The hosted TUI controller still owns
the surrounding `RunWorkflow` transaction inline in `app.rs`:

- cloning the active config and choosing the hosted thread title;
- starting a missing recorded thread and publishing runtime readiness;
- shaping launch rejection and the immediate success event;
- applying the desktop-notification policy after a successful launch.

The runtime later publishes the workflow task projection and terminal
notification. Existing tests cover the typed runtime launch, the controller's
successful and failed saved-workflow paths, and restart hydration, but no test
calls a focused hosted workflow action owner.

This is a process-local transaction boundary. Runtime workflow admission,
task state, transcript persistence, cancellation, and terminal notification
remain runtime-owned.

## Decision

Add a private `hosted_workflow.rs` module with a crate-private
`HostedWorkflowAction` enum and `handle_hosted_workflow_action` entry point.
Move the `RunWorkflow` transaction body there. `app.rs` retains only
`UserAction` selection, action-channel lifecycle, and controller shutdown.

The command enum is process-local and is not a second source of workflow or
task truth. `TuiSurfaceActions::launch_workflow` remains the only mutation
authority.

## Frozen RunWorkflow Ordering

1. Clone the active `RunConfig` before startup or launch work.
2. If no hosted thread exists, start one with the existing title
   `Run saved workflow \`<name>\``. Startup failure emits the same
   `OperationRejected` payload and leaves the prior thread/config/preload state
   unchanged.
3. When startup created the thread, publish the existing runtime-ready events
   before attempting workflow admission.
4. Invoke the typed `launch_workflow` action with the original name and
   optional raw argument string.
5. Launch failure emits the unchanged `OperationRejected` error and emits no
   immediate success or desktop notification. The created thread remains
   installed, matching existing behavior.
6. Successful admission emits `SessionCompleted { status: "success" }`
   immediately. Desktop notification remains conditional on the cloned config
   and uses the existing `Orca` / `Workflow launched` payload.
7. The later workflow task projection and terminal `WorkflowNotification`
   remain asynchronous runtime events and are not fabricated by the owner.

## Failure, Cancellation, And Compatibility

- Disabled-history rejection remains the typed runtime error and stays
  synchronous; the owner does not bypass the runtime surface.
- Workflow execution timeout, retry, cancellation, disconnect, background
  ownership, and terminal notification policy remain runtime/controller-owned
  exactly as before. This slice adds no retry, timeout, or cancellation state.
- Action-channel disconnect still exits the controller and settles hosted
  actors. Restart and durable workflow-task hydration remain unchanged.
- No CLI/slash syntax, `UserAction`, `TuiEvent`, runtime surface, server/JSONL,
  app-server, ACP, transcript, workflow schema, or persistence changes.

## Test Strategy

1. Add a direct owner test that runs the absent owner against a disabled-history
   config and proves the exact typed rejection, no immediate success event, and
   installed-thread behavior without requiring Node workflow execution.
2. Keep the existing successful saved-workflow, failed-launch, typed launch,
   restart hydration, and slash-dispatch tests as behavioral evidence.
3. Add path-specific owner and controller anchors to the runtime contract
   manifest, with deletion-resistant self-tests that cannot pass from imports,
   enum variants, or test-only references.
4. Run focused workflow/owner tests, compiler check, full serial TUI, PTY,
   runtime/Windows validators and self-tests, formatter, and `git diff --check`.
5. Request independent review focused on startup failure, readiness ordering,
   launch rejection, immediate success/notification ordering, runtime task
   ownership, validator integrity, and protocol/storage drift.

## Acceptance Criteria

1. `RunWorkflow` has one transaction owner in `hosted_workflow.rs`; `app.rs`
   only maps the command.
2. The direct owner test is RED before the owner API exists and GREEN afterward.
3. Existing saved-workflow, failure, typed launch, restart, and PTY behavior
   passes unchanged.
4. Contract validation rejects deletion of the production owner or controller
   dispatch while other textual references remain.
5. Full TUI and PTY suites pass after rebase and again on integrated local
   `main`.
6. Independent review has no unresolved Critical or Important finding.

## Implementation Evidence

- At audited base `f9d872b29`, the direct owner test failed because
  `crate::hosted_workflow` did not exist. The focused owner test passes after
  the module and handler were added.
- `hosted_workflow.rs` now owns the complete `RunWorkflow` transaction. The
  controller branch contains only typed command mapping, and the old
  controller failure helper is gone.
- The direct owner test uses a disabled-history config to prove readiness is
  announced before the expected sessionless projection error and exact typed
  rejection, no immediate success event is fabricated, and the startup thread
  remains installed after rejection.
- Focused hosted-workflow, existing saved-workflow/failure/typed-launch/
  restart tests, compiler check, runtime/Windows validators and self-tests,
  formatter, and diff checks pass. Post-extraction source sizes are `app.rs`
  8,834 lines and `hosted_workflow.rs` 132 lines.

## Residual Boundary

Plan implementation, operation/task controls, and interaction control remain
controller-owned action families. Cold legacy registry reconciliation remains
an independent migration boundary.
