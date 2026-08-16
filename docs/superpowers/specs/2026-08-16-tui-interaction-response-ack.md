# TUI Interaction Response Acknowledgement

Status: Implemented and verified on local `main`

## Context

The pending-interaction admission slice validates user/MCP text before sending
`UserAction::RespondToInteraction`, but the submit owner still clears its
pending key, mode, composer, mention bindings, paste payloads, and atomic skill
tokens before the dispatcher knows whether the runtime response committed.
`TuiActionDispatcher` currently reports stale or failed responses as the generic
`TuiEvent::OperationRejected`, which moves the reducer to `Idle` without enough
identity or composer data to retry the same interaction.

This creates a renderer-only loss window for runtime-unavailable,
deferred/uncommitted, or transport failures after local admission. A stale
interaction must retain its existing terminal rejection behavior; it must not be
reintroduced as a new pending interaction.

## Decision

Add a crate-private, bounded non-blocking acknowledgement lane between the TUI action
dispatcher and the frame loop:

- `RespondToInteraction` produces `Committed`, `NoLongerPending`, or `Failed`
  acknowledgement records without changing `UserAction` or `TuiEvent`.
- `AppState` retains at most one key-matched pending interaction submission
  snapshot while its optimistic transition is in flight.
- A committed acknowledgement discards the matching snapshot.
- A failed acknowledgement restores the matching pending key/mode, exact
  composer text, mention bindings, pending paste payloads, and atomic skill
  tokens, reports the existing error, and returns to `WaitingUserInput`.
- A stale/mismatched acknowledgement discards the snapshot and preserves the
  existing `OperationRejected`/`Idle` behavior.
- A newer runtime interaction, terminal completion, or session reset retires an
  older in-flight snapshot before it can restore stale input.

The acknowledgement lane is internal to `orca-tui`; runtime response mutation,
interaction fencing, retries, disconnect semantics, and terminal settlement
remain owned by `TuiSurfaceTaskControl` and the runtime surface.

## Frozen Behavior

1. Successful user and MCP responses keep the current optimistic Running
   transition and visibly clear composer/pending state immediately; the private
   retry snapshot is discarded after the committed ack.
2. A runtime failure after local admission restores the exact pre-submit
   interaction and composer state, appends the existing error text, and leaves
   the TUI in `WaitingUserInput` for retry.
3. A response whose local binding is gone remains a terminal rejection and does
   not restore stale input.
4. Approval, permission, task, operation, ordinary submit, and queued-submit
   actions keep their current event/error behavior.
5. Only one direct interaction response can be in flight in the real TUI. The
   acknowledgement lane also covers at most one auto-response for each of the
   256 runtime events processed per frame plus the 64-value action mailbox, so
   its capacity is fixed at `256 + 64 + 1 = 321`.

## Compatibility And Ownership

- `UserAction`, `TuiEvent`, runtime surface commands/events, server/JSONL, ACP,
  CLI/slash syntax, persistence, transcript formats, and public Rust API are
  unchanged.
- The new result type, retry snapshot, and dispatcher receiver are
  crate-private. The public `AppState::pending_input` shape is unchanged.
- The dispatcher remains the only owner of action routing; the frame loop is
  the only owner of presentation restoration; the runtime remains the only
  response mutation owner.
- The acknowledgement channel uses bounded `try_send`; Interrupt, Cancel, and
  shutdown never wait for the frame loop to drain acknowledgements. Exceeding
  the 321-value legal production bound emits an explicit internal error instead
  of blocking or allocating further memory.

## Test Strategy

1. Add RED tests proving a failed accepted response currently cannot restore
   pending input/composer state.
2. Add dispatcher tests for committed, stale, and failed response
   acknowledgements while preserving existing approval behavior.
3. Add state/renderer tests for exact retry restoration, key matching, and
   retirement by a newer interaction, terminal completion, and reset.
4. Keep existing canonical interaction, pending-input, action-dispatcher,
   Side, restart, and PTY suites as downstream evidence.
5. Run focused tests, compiler check, full serial TUI, PTY, runtime/Windows
   validators and self-tests, formatter, diff checks, and independent review.

## Acceptance Criteria

1. A failed admitted user/MCP response restores the exact pending interaction
   and composer state with no public event/action contract change.
2. A committed response cannot restore the old composer after a later runtime
   event.
3. Stale or mismatched responses remain terminal rejections and do not revive
   an interaction.
4. Dispatcher shutdown and full action mailboxes remain non-blocking.
5. All existing external/public/runtime/persistence contracts remain unchanged.
6. Full TUI and PTY suites pass after rebase and on integrated local `main`.
7. Independent review has no unresolved Critical or Important finding.

## Implementation Evidence

- At audited base `d181d596e`, the RED tests failed because no crate-private
  acknowledgement type or frame-loop handler existed. The renderer could only
  consume generic `OperationRejected` events after its pending key and composer
  were already gone.
- The dispatcher now maps each prioritized interaction response to exactly one
  bounded non-blocking acknowledgement: committed, no-longer-pending, or
  failed. The command mailbox remains bypassed, legal consecutive
  acknowledgements fit the 321-value scheduling bound, and undrained
  acknowledgements do not block Interrupt, Cancel, or shutdown.
- The submit owner snapshots visible text, mention bindings, atomic skill
  tokens, and pending paste payloads before paste expansion. Focused tests prove
  exact restoration on failure, terminal stale rejection, commit retirement,
  newer-interaction fencing, and unchanged approval rejection behavior.
- Session completion and projection reset tests prove the in-flight snapshot is
  retired with the pending key/mode. Existing seven submit-owner tests, eleven
  dispatcher tests, three canonical approval/permission/user-input fence tests,
  and exact waiting user/MCP response tests pass. The dispatcher suite proves
  two queued responses retain both acknowledgements, undrained acknowledgements
  do not block Interrupt, shutdown never waits for their consumption, and a
  forged 322-response burst is capped with an explicit error while Interrupt
  still succeeds.
- CodeRabbit's first review raised one Major issue for the initial ignored
  full-lane send; a RED regression reproduced the missing second
  acknowledgement. Its second review correctly found that the interim bounded
  backpressure could stall Interrupt. A second RED regression reproduced that
  control delay. Its third review correctly rejected the mechanically unbounded
  interim lane; the final capacity derives from the renderer's bounded event
  batch and action mailbox, closes all three Major issues, and preserves
  prioritized control. The same review's Vim Minor was already satisfied
  because `reset_insert()` cancels the pending command; the focused restoration
  test now asserts that behavior. A fourth review attempt after the bounded fix
  was rejected by the service's three-review rate limit, so no later CodeRabbit
  result is claimed.
- The runtime-surface validator now freezes the production frame-loop ack drain;
  its negative self-test rejects deletion while test and enum references remain.
  Manifest SHA is `d2b2ac479a15ff23e1bd567009b102994844da0df3f82d57fc5c4bfce1a0d5f4`.
- Compiler check passes. Current source sizes are `app.rs` 8,842 lines,
  `action_dispatcher.rs` 815, `agent_runtime.rs` 262,
  `idle_submit_actions.rs` 470, `runtime_event_actions.rs` 1,248, and
  `types.rs` 8,936 lines.
- Final pre-commit topic gates pass: serial `orca-tui` library tests
  1,110/1,110 in 43.01 seconds, root-package PTY contracts 6/6 in 9.47
  seconds, runtime and Windows validators plus both negative self-test suites,
  `cargo check -p orca-tui --tests --locked`, formatter, and diff checks.
- The no-op rebase onto current local `main` repeated 1,110/1,110 TUI tests,
  6/6 PTY contracts, both validators, formatter, and diff checks. Integrated
  local `main` then passed 1,110/1,110 TUI tests in 269.66 seconds, 6/6 PTY
  contracts in 9.75 seconds, both validators and self-test suites, formatter,
  and diff checks. The slice worktree and merged branch were removed while all
  unrelated worktrees remained registered.

## Residual Boundary

An uncommitted response can still be unretryable for a server-side semantic
reason, so an explicit retry may receive the same rejection. This slice does
not add a second broker, schema validation, or automatic retry; it only
preserves the single existing interaction for an explicit user retry.
