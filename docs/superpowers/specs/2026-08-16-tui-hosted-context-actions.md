# TUI Hosted Context Action Ownership

Status: Implemented on `codex/tui-hosted-context-actions`

## Context

At audited base `4cce63ec0`, runtime surface actions already own the actual
memory, manual-compaction, and backtrack mutations. The hosted TUI controller
still owns their transaction policy inline in `app.rs`:

- starting a recorded thread before a Remember action;
- ordering `RuntimeReady`, memory persistence, pinned-context mutation, Notice,
  and partial-success errors;
- shaping empty-state and runtime errors for manual compaction;
- translating backtrack results into prompt restoration or exact errors.

These branches are one context-mutation family, but their user-visible event
ordering and failure semantics have no focused owner. `app.rs` is 8,973 lines;
the three branches occupy roughly 75 lines. Existing tests cover typed memory
persistence and pinning, manual-compaction lifecycle/cancellation, renderer
backtrack state, and slash-command dispatch, but no test calls a focused hosted
context action owner.

This is a boundary defect rather than a runtime mutation defect. Mutation facts
remain runtime-owned; only process-local TUI transaction ownership moves.

## Decision

Add a private `hosted_context.rs` module with a crate-private
`HostedContextAction` enum and one `handle_hosted_context_action` entry point.
Move Remember, Compact, and Backtrack transaction bodies there. `app.rs`
retains `UserAction` selection, Side-only action restrictions, action-channel
termination, and final controller shutdown.

The command enum is process-local and is not a second source of context,
memory, compaction, or transcript truth. `TuiSurfaceActions` and the typed
runtime surface remain the mutation authority.

## Frozen Remember Ordering

1. Build pinned context from the trimmed note and snapshot the active config.
2. If no thread exists, start/preflight the hosted thread with the existing
   `Remembered context` title. Startup failure emits the same raw `Error`,
   preserves preload state, and performs no memory or pin mutation.
3. If startup created the thread, publish `RuntimeReady` before the memory
   mutation.
4. Resolve the memory cwd from configured cwd or the current-directory
   fallback, then invoke the typed user/project memory mutation with the
   original note.
5. On memory success, emit `Notice("Remembered in <path>.")` before attempting
   the pinned-context mutation.
6. Pin failure preserves the already-saved memory and emits
   `Error("memory was saved but could not be pinned: ...")`. Memory failure
   emits `Error("failed to remember: ...")` and does not attempt pinning.
7. A thread created for Remember remains installed after a later memory or pin
   failure, matching current behavior.

## Frozen Compact Ordering

- Without an active thread, emit `Error("nothing to compact")` and do nothing
  else.
- With a thread, call the typed manual-compaction action with the existing
  `TuiSurfaceTaskControl` and attached event sender.
- Runtime lifecycle/projection events remain runtime-surface-owned. Immediate
  failure emits `OperationRejected("manual compaction failed: ...")`.
- Cancellation, durable completion, interrupted recovery, and compaction
  timeout/retry policy remain runtime-owned and unchanged.

## Frozen Backtrack Ordering

- Invoke typed `backtrack_last_user` only when a thread exists.
- A restored prompt emits exactly `Backtracked { prompt }`.
- No thread or no backtrackable prompt emits `Error("nothing to backtrack")`.
- Runtime failure emits `Error(error.to_string())` without fabricating a
  restored prompt.
- Submitted-turn backtrack eligibility and durable history mutation remain
  runtime-owned.

## Failure, Disconnect, Restart, And Compatibility

- Remember and Backtrack remain synchronous typed surface mutations with no new
  retry, timeout, cancellation, or approval policy.
- Manual compaction continues to use runtime cancellation and lifecycle
  recovery. This slice adds no resettable cancellation state or worker.
- Event-channel send failure remains best-effort as before. Action-channel
  disconnect still exits the controller and settles hosted actors.
- Restart behavior remains unchanged: memory and compaction durability follow
  their existing runtime stores; no process-local command or event becomes
  durable.
- No CLI/TUI command, `UserAction`, `TuiEvent`, runtime surface, server/JSONL,
  app-server, ACP, transcript, memory, or session-store schema changes.

## Test Strategy

1. Add a direct `hosted_context` test that calls the absent owner for Compact
   and Backtrack without a thread. It must prove exact error order and unchanged
   thread/config/preload state.
2. Keep typed memory/pinning, compaction lifecycle/cancellation, renderer
   backtrack, and slash-dispatch tests as behavioral evidence.
3. Add the owner and controller mapping to each manifest action row. Add
   path-specific validation/self-tests that cannot pass from enum variants,
   imports, or test-only calls.
4. Run focused hosted-context, memory, compaction, backtrack, and controller
   tests; compiler check; full serial TUI; PTY; runtime/Windows validators and
   self-tests; formatter; and `git diff --check`.
5. Request independent review focused on startup/preload preservation,
   partial-success memory/pin ordering, compaction control ownership,
   backtrack prompt/error shaping, validator integrity, and protocol/storage
   drift.

## Acceptance Criteria

1. Remember, Compact, and Backtrack have one transaction owner in
   `hosted_context.rs`; `app.rs` only maps commands.
2. The direct owner test is RED before the API exists and GREEN afterward.
3. Existing memory, compaction, backtrack, and PTY behavior passes unchanged.
4. Contract validation rejects deletion of each production owner branch or
   controller dispatch while other textual references remain.
5. Full TUI and PTY suites pass after rebase and again on integrated local
   `main`.
6. Independent review has no unresolved Critical or Important finding.

## Implementation Evidence

- At audited base `4cce63ec0`, the direct owner test failed because
  `crate::hosted_context` did not exist. The focused owner tests pass after the
  module and handler were added.
- `hosted_context.rs` now owns all three transaction bodies. The controller
  branches contain only typed command mapping and one owner call.
- The Remember owner test starts a real recorded thread, proves
  `MentionRuntimeReady` precedes the success Notice, reads the saved user
  memory, and verifies the pinned context through a fresh typed attachment.
- The empty Compact/Backtrack owner test proves exact error order and unchanged
  thread, config, and preload state.
- Focused Remember, manual-compaction, backtrack, and hosted-context tests pass.
  The compiler check, 1,088-test serial TUI suite, six PTY contracts, runtime
  and Windows validators and self-tests, formatter, and diff check also pass.
  Post-extraction source sizes are `app.rs` 8,858 lines and `hosted_context.rs`
  252 lines.

## Non-Goals

- Changing memory scope, file placement, pin semantics, or automatic memory.
- Changing compaction policy, summaries, cancellation, recovery, or provider
  behavior.
- Changing which submitted turns are backtrackable or how the renderer inserts
  a restored prompt.
- Extracting plan implementation, workflow launch, operation/task controls, or
  the whole hosted controller loop.
- Reconciling cold legacy registry-only task records.

## Rollback

Revert the single semantic commit. No schema or data migration is involved.

## Residual Boundary

Hosted plan implementation, workflow launch, and operation/task action
transactions remain controller-owned. Cold registry reconciliation remains an
independent migration boundary.
