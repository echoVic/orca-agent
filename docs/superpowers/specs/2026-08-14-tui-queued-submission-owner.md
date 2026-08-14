# TUI Convergence Slice 12: Queued Submission Aggregate Ownership

## Status

Implemented and verified on `codex/tui-queued-submission-owner`, based on local
`main` at `3264c6b62` after TUI input-history ownership was integrated and
verified. Independent review found no Critical or Important issue; its four
Minor test gaps were added before the full gates passed.

## Problem And Evidence

`crates/orca-tui/src/queued_input.rs` owns `QueuedUserMessage`, composer restore,
and preview types, but `AppState` exposes five separate queue facts in
`types.rs`: pending messages, the in-flight message, autosend, the last queue
error, and the next admission id. Eleven transition methods also live in
`types.rs`, while production code in `queued_input_actions.rs`, `ui.rs`, and the
`TuiEvent` reducer directly reads or writes individual queue fields.

This is an architecture-boundary defect. The queue has one conceptual state
machine but multiple mutation authorities. A previous unmerged branch only
moved the eleven methods and widened `next_queued_submission_id` to
`pub(crate)`; that changes source location while preserving the ownership
defect. Current `main` is authoritative, and this slice does not merge or edit
that stale worktree.

The reliability risk is concrete: channel-full/channel-closed dispatch currently
performs rollback and error assignment as two separate public mutations, and a
matching runtime start clears the in-flight fence by writing the field from the
general reducer. Every required transition should be one queue-owner operation.

## User Value And Scope

Queued follow-ups must remain FIFO, never disappear when the bounded action
channel rejects a send, never execute twice while one admission is fenced, and
remain visibly recoverable through the queue preview. This slice:

- adds private `QueuedSubmissionState` in `queued_input.rs` as the sole owner of
  pending, in-flight, autosend, error, and next-id facts;
- replaces the five AppState fields with one `queued_submission` aggregate;
- moves the used AppState queue methods next to that owner and retires the old
  standalone rollback method after replacing its test-only caller with the
  atomic dispatch-failure transition;
- adds intent methods for matching-start settlement, atomic dispatch failure,
  capacity error, and a read-only preview/error projection;
- removes every production direct read or write of an individual queue fact;
- moves the core queued admission behavior tests from `types.rs` to the owner
  module and updates other tests to use behavior/query helpers.

`AppState` remains responsible for global TUI status, transcript mutations,
input history recording, and constructing `UserAction::SubmitQueued`.
`queued_input_actions.rs` remains responsible for attempting the bounded
channel send. The aggregate owns no channel, task, thread, connection, or
cancellation token.

## State And Transition Contract

`QueuedSubmissionState` starts with an empty pending queue, no in-flight
message, autosend enabled, no error, and next id `1`.

- Enqueue rejects over-capacity input unchanged, otherwise assigns a nonzero
  wrapping id, appends FIFO, and clears any previous error.
- Begin is admitted only when autosend is enabled and no message is in flight.
  It removes the FIFO head, stores that exact message as the in-flight fence,
  and returns a clone for AppState transcript/action construction.
- Successful channel admission records input history but retains the fence
  until the matching `QueuedSubmissionStarted { id }` arrives.
- A matching start consumes the fence; a stale or unrelated id changes nothing.
- Channel-full and channel-closed failures atomically move the in-flight message
  back to the FIFO front and record the user-visible error. The message is not
  recorded in input history and optimistic transcript state is removed by
  AppState as today.
- A matching runtime rejection retains the fence until composer restoration;
  restoration consumes it, disables autosend, clears the queue error, and
  restores the exact visible text, mention bindings, and pending pastes.
- Pop-latest is LIFO editing of pending messages only and clears a prior error.
- Reset clears pending, in-flight, and error, reenables autosend, and resets the
  next id to `1`.

## Cancellation, Failure, And Recovery Semantics

- Cancel or interrupt suspends autosend before sending the existing typed
  interrupt action. Pending messages and any current fence remain owned and are
  not submitted automatically.
- Permission denial and plan rejection keep their existing surface semantics;
  they only resume or suspend queued autosend through the owner API.
- This state machine owns no timer. A provider/tool timeout after
  `QueuedSubmissionStarted` belongs to the admitted runtime operation; the
  queued fence is already settled and is not replayed.
- A full or disconnected local action channel is a pre-admission failure: the
  message returns to the front with an exact visible error and can be retried by
  a later dispatch boundary.
- A stale rejection id cannot consume or restore the live queued fence.
- Queue state is intentionally process-local. Restart constructs the empty
  default aggregate; persisted session history and external side effects are
  unchanged and no queued message is claimed durable.

## Ownership And Compatibility

`queued_input.rs` is the unique fact and transition owner. `types.rs` contains
only the aggregate field and coordinates queue transitions with global AppState
facts. `ui.rs` receives an owned bounded snapshot instead of the underlying
deque or error field. Test-only queries may expose counts, visible text, error,
autosend, and fence identity; they may not return mutable queue state.

There is no CLI argument, TUI key flow, `UserAction`, runtime event, server/JSONL
protocol, history JSONL, SQLite schema, or persisted session format change.
Used method names remain source-compatible inside the crate. The obsolete
standalone rollback method is deleted with its test-only caller migrated to the
real failure transition. No compatibility wrapper or second queue cache remains
after migration.

## Acceptance

1. One owner-level RED test first names the absent aggregate behavior: beginning
   the first of two messages and failing dispatch must atomically restore FIFO,
   clear the fence, retain autosend, and publish the supplied error.
2. `AppState` contains one `QueuedSubmissionState` field and no individual
   queued-message, in-flight, autosend, error, or next-id fields. Production code
   outside `queued_input.rs` has no direct access to those facts.
3. Existing queue behavior remains green, including capacity, FIFO promotion,
   LIFO edit, admission fence, matching/stale starts and rejections, full/closed
   channel rollback, cancel suspension, plan resume, reset, and preview bounds.
4. Focused owner and dispatch suites pass:
   `cargo test -p orca-tui queued_ --lib --locked -- --test-threads=1` and
   `cargo test -p orca-tui queued_input --lib --locked -- --test-threads=1`.
5. TUI full and PTY gates pass:
   `cargo test -p orca-tui --lib --locked -- --test-threads=1` and
   `cargo test --test tui_pty_contract --locked -- --test-threads=1`.
6. Runtime-surface and Windows validators pass after relocating only the
   reviewed inventory keys for queue admission/history recording and reset.
7. `cargo fmt --all -- --check`, `git diff --check`, an obsolete-field search,
   and independent code review show one queue owner with no protocol or
   persistence change.

## Migration, Deletion, And Rollback

The aggregate, delegates, call-site migration, old-field deletion, and
inventory refresh land in one semantic commit. There is no dual state and no
temporary adapter. The old definitions and five fields are deleted in the same
slice, including the now-unused standalone rollback method. One commit revert
restores the previous process-local layout;
no data migration or recovery is required.

## Spec Self-Review

There are no placeholders. Normal admission, cancellation, denial interaction,
timeout ownership, channel failure, retry, stale events, reset, and restart are
defined. Queue facts, global TUI coordination, channel dispatch, cancellation,
and persistence owners are distinct. Every behavior and deletion condition has
an executable acceptance gate, and the slice is independently reviewable and
revertible.
