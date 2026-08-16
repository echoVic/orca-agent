# TUI Hosted Side Lifecycle Action Ownership

Status: Implemented on `codex/tui-hosted-side-actions`

## Context

`hosted_side.rs` already owns the attached parent/Side state, active-config
selection, parent status projection, Side sender rotation, and child-before-
parent shutdown. The hosted controller still implements all three Side action
transactions inline:

- start Side conversation;
- toggle between parent and Side;
- close Side conversation.

Those branches occupy roughly 330 lines in `app.rs` and jointly own candidate
startup, projection preflight, attachment generation changes, deferred parent
event replay, background presentation rebind, bounded child shutdown, and
parent restoration. The lifecycle facts and the action transaction therefore
have different implementation owners.

At audited base `cda7ec106`, `app.rs` is 9,266 lines and `hosted_side.rs` is 96
lines. Existing tests cover recorded-parent versus ephemeral-Side identity,
parent/Side transcript restoration, stale attachment fencing, background task
presentation rebind after Side reentry, child-before-parent controller-exit
shutdown, and two PTY Side workflows. There is no direct owner test for a Side
start requested before a parent thread exists.

## Decision

Add a crate-private `HostedSideAction` command enum and one
`handle_hosted_side_action` entry point to `hosted_side.rs`. Move the three
controller transaction bodies into that owner. `app.rs` retains public
`UserAction` selection, Side-only action restrictions, action-channel
termination, and final controller shutdown.

Move the generic `rotate_attached_event_sender` helper from `hosted_side.rs` to
`attachment_routing.rs`. Session lifecycle already uses that helper, while the
new Side owner must call session preflight/reaping. Relocating the generic
attachment helper preserves a one-way production dependency instead of adding
a `hosted_side`/`hosted_session_lifecycle` cycle. Side-specific sender rotation
remains in `hosted_side.rs`.

The command enum is process-local. It does not replace runtime thread identity,
attachment generation, the typed runtime surface, or persisted session state.

## Frozen Start Ordering

1. Reject an already-open Side or a missing parent before mutating ownership.
2. Take the parent thread, read its status/title, and derive an ephemeral Side
   config with disabled history, disabled auto-memory, and Plan approval mode.
3. Start and preflight the Side candidate, then read its complete projection
   batch. Any failure restores the parent; projection-read failure also reaps
   the candidate.
4. Create the next attachment sender, install `HostedSideParent`, make the Side
   thread/sender/attachment active, and switch routing.
5. Publish reset/history, `SideConversationChanged`, `RuntimeReady`, and the
   existing notice in that order. An optional initial prompt is submitted only
   after the Side projection is active.
6. A reset/history send failure emits the existing `Error` after installation;
   it does not roll back the installed Side.

## Frozen Toggle Ordering

- Without an attached Side, toggle is a no-op.
- Read the complete target projection before changing the active thread,
  sender, attachment, or routing. Read failure leaves the source active and
  emits the existing `OperationRejected` on its attached channel.
- Side to parent uses deferred routing, publishes reset/history, then releases
  queued parent interactions before publishing the inactive Side status and
  parent `RuntimeReady`.
- Parent to Side first retires and rotates the hidden Side sender, activates
  the new generation, publishes reset/history, rebinds background presentation
  monitors, then publishes active Side status and `RuntimeReady`.
- Projection-send failure happens after the active target changes, emits the
  existing `Error`, and does not roll back or publish later status/ready steps.
- Background presentation rebind failure emits the existing
  `OperationRejected` but does not undo the switch.

## Frozen Close Ordering

- Without an attached Side, close is a no-op.
- If Side is active, read the parent projection before child shutdown. Failure
  restores the `HostedSideParent` value and keeps Side active.
- Shut down the Side actor with the existing five-second bound. Failure restores
  the full Side state and emits the existing `OperationRejected`.
- If Side was active, restore the parent thread/sender/attachment, activate
  deferred routing, publish parent reset/history, and release parent events.
  Parent projection-send failure emits `Error` but does not stop cleanup.
- If the parent was already active, retain its sender/attachment and only
  retire Side routing.
- Publish Side unavailable and parent `RuntimeReady` after successful child
  shutdown. Final controller exit still joins child before parent.

## Failure, Restart, And Compatibility

- Startup, preflight, projection-read, projection-send, rebind, and shutdown
  errors keep their exact current channel, text prefix, and rollback boundary.
- No new retry or timeout policy is introduced. Candidate reaping and the
  five-second close/exit shutdown bounds remain unchanged.
- Runtime/provider disconnect behavior remains runtime-owned. Action-channel
  disconnect still exits the controller and settles the attached actors.
- Side remains ephemeral across process restart; only the recorded parent can
  resume. No Side transcript, command, or attachment id becomes durable.
- No CLI/TUI command, `UserAction`, `TuiEvent`, runtime surface, server/JSONL,
  app-server, ACP, transcript, or session-store schema changes.

## Test Strategy

1. Add a direct `hosted_side` test that calls the absent owner for Start with no
   parent and proves the exact attached-channel rejection plus unchanged
   thread/Side/attachment/config/preload state.
2. Keep the recorded parent/ephemeral Side, reentry, stale attachment,
   foreground, controller-exit, and PTY tests as behavioral ordering evidence.
3. Update the three Side manifest source references to the new owner and
   controller dispatch. Add path-specific owner/dispatch validation that cannot
   pass from imports or test-only calls.
4. Run focused Side/routing tests, `cargo check`, full serial TUI, PTY,
   runtime/Windows validators and self-tests, formatter, and `git diff --check`.
5. Request independent review focused on candidate rollback, generation
   rotation, deferred replay, rebind ordering, shutdown recovery, validator
   integrity, and public/persistence drift.

## Acceptance Criteria

1. The three Side action transactions have one implementation owner in
   `hosted_side.rs`; `app.rs` only maps commands.
2. The direct owner test is RED before the API exists and GREEN afterward.
3. Existing Side identity, attachment, background task, shutdown, and PTY tests
   pass unchanged.
4. Contract validation rejects deletion of the production owner or controller
   dispatch while imports and test calls remain.
5. Full TUI and PTY suites pass after rebase and again on integrated local
   `main`.
6. Independent review has no unresolved Critical or Important finding.

## Implementation Evidence

- At audited base `cda7ec106`, the direct Side-owner test first failed because
  `HostedSideAction` and `handle_hosted_side_action` did not exist. It passes
  after the owner was added.
- The three controller branches now contain only command mapping and one owner
  call. Start, toggle, and close transactions live in `hosted_side.rs`.
- The generic attached-sender rotation helper now lives in
  `attachment_routing.rs`; the only reverse reference from that module to Side
  is inside its test module.
- The focused Side cluster passes 24 tests and the attachment-routing cluster
  passes 4 tests. `cargo check -p orca-tui --tests --locked` also passes.
- Each Side manifest row references its exact owner branch and controller
  mapping. Path-specific validator self-tests reject deletion of any of the
  six production sites while imports, enum variants, or tests remain.
- Independent review found no Critical or Important issue. Candidate and close
  failure injection remains a minor test gap; those failure bodies are direct
  relocations, while the successful state machine, routing barriers, stale
  generations, reentry, and final shutdown joins have behavioral coverage.
- Post-extraction source sizes are `app.rs` 8,973 lines, `hosted_side.rs` 495
  lines, and `attachment_routing.rs` 485 lines.

## Non-Goals

- Extracting the entire hosted controller loop or its final action-channel
  ownership.
- Changing Side availability rules, inherited history, approval mode, or
  persistence.
- Changing attachment generations, deferred replay semantics, task
  foregrounding, or background presentation ownership.
- Reconciling cold legacy registry-only task records.

## Rollback

Revert the single semantic commit. No schema or data migration is involved.

## Residual Boundary

Hosted workflow/plan, memory/compaction/backtrack, and operation/task action
transactions remain controller-owned. Cold registry reconciliation remains
independent migration work.
