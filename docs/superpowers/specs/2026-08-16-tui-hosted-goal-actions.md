# TUI Hosted Goal Action Ownership

Status: Implemented on `codex/tui-hosted-goal-actions`, based on clean local
`main` at `88f3eb241`.

## Context

The TUI convergence series has given hosted Goal reads and turn orchestration a
focused `hosted_goal` module, while session creation/replacement and
latest-active Goal recovery live in `hosted_session_lifecycle`. The controller
still embeds the six Goal action transactions (`show`, `set`, `edit`, `clear`,
`pause`, and `resume`) as roughly two hundred lines of `app.rs` match arms.

Those arms do more than dispatch. They decide whether a runtime thread must be
started, preserve the ordering of runtime-ready and Goal events, restore a
preloaded recorded session before mutation, publish committed projections, and
shape operation errors. Leaving those transactions in the renderer controller
splits Goal ownership across three files and keeps future lifecycle changes
coupled to the broad action loop.

At the audited base `88f3eb241`, `app.rs` is 9,518 lines and
`hosted_goal.rs` is 155 lines. Existing integration tests cover empty-session
show/resume, latest-active recovery, preloaded edit/clear/pause/show, active
pause/clear, queued set interruption, and no-progress continuation behavior.

## Decision

Add a crate-private `HostedGoalAction` command enum and one
`handle_hosted_goal_action` transaction entry point to `hosted_goal.rs`. Move
the existing six Goal action bodies into that entry point. `app.rs` retains the
public `UserAction` match and maps each Goal variant to the focused command.

`HostedGoalAction` is not persisted, serialized, projected, or exposed outside
`orca-tui`; it is only a typed call boundary. Runtime surface Goal state remains
the sole Goal fact source.

The owner receives the existing thread slot, runtime host, shared config,
preloaded transcript, event channel, surface task control, and pending workflow
notification handle. It composes existing `hosted_goal`,
`hosted_session_lifecycle`, `hosted_session`, and `surface_actions` APIs without
adding a compatibility wrapper.

After implementation, `app.rs` is 9,385 lines and `hosted_goal.rs` is 404
lines. These counts are evidence of the relocation, not acceptance criteria.

## Frozen Behavior

- Show resolves the current live/preloaded session exactly as before and emits
  either `GoalStatus`, the recorded-history error, or the existing read error.
- Set starts a missing hosted thread, announces runtime readiness only for a
  newly started thread, emits the existing start notice, then invokes the typed
  Goal mutation. Goal-run failures retain existing operation-error shaping.
- Edit, clear, and pause validate that a recorded session identity exists
  before starting a missing thread. A restored thread is announced before the
  mutation result. Success publishes the returned
  `SurfaceProjectionSynced`; errors retain their existing text.
- Edit uses the same current Unix timestamp semantics.
- Resume without a current session identity delegates to the existing
  latest-active recovery transaction. Resume with an identity reads the
  current Goal and launches the same continuation prompt only when that Goal is
  present. Errors retain Goal-run operation shaping.
- Missing-thread startup rejection for set/edit/clear/pause remains
  `OperationRejected`. Latest-active resume startup/load/projection failures
  retain their existing `Error` events in the session-lifecycle owner.
- Provider/runtime timeout, retry, and disconnect policy remains owned by the
  runtime surface and Goal actor. This handler adds no retry loop, deadline, or
  disconnect translation; it forwards the existing operation outcome through
  `emit_hosted_operation_error`. Action-channel disconnect still exits through
  the controller's existing final-shutdown path.
- Restart recovery remains lifecycle-owned: latest-active Goal discovery,
  transcript load, candidate start and validation, previous-thread retirement,
  recovered-approval publication, and continuation launch stay one ordered
  transaction. Candidate failure does not retire the previous thread.
- Controller Side restrictions, attachment routing, queued action scheduling,
  non-Goal action dispatch, and final thread shutdown remain in `app.rs`.

## Compatibility

- No CLI, TUI command, server JSONL, app-server, persistence, or runtime surface
  schema changes.
- `UserAction` and `TuiEvent` variants remain unchanged.
- No Goal state is copied into the controller command enum.
- Existing error text, event order, cancellation behavior, and continuation
  semantics remain unchanged.

## Test Strategy

1. Add a direct module test that calls the new Goal action owner for an empty
   recorded session and proves `Show` emits `GoalStatus(None)` without starting
   a runtime thread.
2. Keep the existing controller integration tests as behavioral coverage for
   all six Goal actions, preloaded restoration, latest-active recovery,
   cancellation, and queued interruption.
3. Run focused Goal/hosted-goal tests, `cargo check`, the full serial TUI suite,
   PTY contracts, runtime/Windows validators and self-tests, formatter, and
   `git diff --check`.
4. Request independent review focused on event ordering, recovery delegation,
   runtime-thread lifecycle, validator integrity, and accidental public or
   persistence changes.

## Acceptance Criteria

1. The six Goal action transactions have one implementation owner in
   `hosted_goal.rs`; `app.rs` contains only action-to-command mapping.
2. The direct owner test is RED before the API exists and GREEN after the
   extraction.
3. Existing Goal behavioral tests pass without changing their expectations.
4. Source/protocol validators remain meaningful and pass with updated anchors
   only where the relocation requires it.
5. Full TUI and PTY suites pass after rebase and again on integrated local
   `main`.
6. Independent review finds no unresolved Critical or Important issue.

## Implementation Evidence

- The direct owner test first failed to compile because `HostedGoalAction` and
  `handle_hosted_goal_action` did not exist. After implementation it passes.
- The focused `goal_` filter passes 40 tests, covering all existing Goal
  controller behavior plus the new direct owner test.
- `cargo check -p orca-tui --tests --locked` passes.
- The full serial TUI suite passes 1,084/1,084 and the root PTY contract passes
  6/6 in the topic worktree.
- The runtime validator now anchors the controller's call-shaped
  `handle_hosted_goal_action(` dispatch and uses path-specific definition
  anchors for the action owner and latest-active recovery owner. Negative
  self-tests remove all six production dispatch calls while preserving the
  import, and separately remove the action owner while preserving calls; both
  mutations fail validation.
- The Windows boundary validator passes unchanged. No public protocol,
  persistence, or runtime surface inventory changed.
- Runtime and Windows validator self-tests, `cargo fmt --all -- --check`, and
  `git diff --check` pass.
- Independent review found no Rust behavior regression, but reported two
  Important evidence gaps: the shared callback regex did not prove the new
  owner definition remained, and the Spec Gate did not enumerate required
  startup/timeout/retry/disconnect/restart ownership. Path-specific anchors,
  an owner-removal negative self-test, and the frozen failure semantics above
  resolve both findings.

## Non-Goals

- Extracting the full hosted controller loop or Side attachment lifecycle.
- Changing Goal persistence, continuation policy, retry limits, or status
  semantics.
- Removing legacy pending-interaction compatibility paths.
- Adding new user-visible Goal commands or messages.

## Rollback

Revert the single semantic commit. Because the change is an internal ownership
extraction with no schema or persistence migration, rollback restores the six
controller bodies without data conversion.

## Residual Boundary

After this slice, Side lifecycle and the remaining non-Goal controller action
families still need separate evidence before extraction. Cold legacy registry
reconciliation remains independent migration work. `hosted_goal` now composes
the session-lifecycle APIs while that module consumes Goal prompt/error helpers;
this traceable but bidirectional module dependency should be removed by a later
boundary rather than hidden with another compatibility wrapper.
