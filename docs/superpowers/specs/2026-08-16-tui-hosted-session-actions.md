# TUI Hosted Session Action Ownership

Status: Implemented on `codex/tui-hosted-session-actions`

## Context

`hosted_session_lifecycle.rs` owns candidate thread startup, identity preflight,
installation, replacement, reaping, new/fork/saved-session switching, and saved
picker refresh. The hosted controller still embeds the eight action
transactions that compose those APIs with attachment rotation, reset/history
projection, runtime-ready publication, and saved-session metadata outcomes:

- new session;
- fork current session;
- rename current session;
- resume saved session;
- fork saved session;
- rename saved session;
- archive saved session;
- delete saved session.

This leaves one lifecycle operation split between its owner and roughly 235
lines of the renderer controller. It also makes the attachment and projection
ordering harder to review because the same sequence is repeated for new,
resume, and both fork paths.

At audited base `9d621f1b9`, `app.rs` is 9,385 lines and
`hosted_session_lifecycle.rs` is 447 lines. Existing tests cover new/fork/resume
projection, active-work rejection, preflight failure preservation, durable
rename success/failure, picker fork/resume behavior, and current-session action
availability. Before this slice, no test invoked the archive/delete owner guard.

## Decision

Add a crate-private `HostedSessionAction` command enum and one
`handle_hosted_session_action` entry point to
`hosted_session_lifecycle.rs`. Move the eight existing controller bodies into
that owner. `app.rs` retains public `UserAction` selection and maps each session
variant into the focused command.

The command enum is process-local and contains only action parameters. It is
not a session fact, is never persisted or projected, and does not replace the
runtime thread/session identity. The handler receives the existing thread slot,
runtime host, shared config/preload, pending workflow notifications, root and
attached event senders, attachment id, and routing authority.

## Frozen Success Ordering

- New: candidate start/preflight/install, attached sender rotation,
  `SessionProjectionReset`, `RuntimeReady`, then `NewSessionStarted`.
- Fork current: candidate start/preflight/install, rotation, reset,
  `RuntimeReady`, then typed history with `Forked` presentation.
- Resume saved: transcript load plus candidate start/preflight/install,
  rotation, reset, `RuntimeReady`, then typed history without fork
  presentation.
- Fork saved: transcript load plus candidate start/preflight/install, rotation,
  reset, `RuntimeReady`, then typed history with `Forked` presentation.
- Rename current publishes the post-commit `SurfaceProjectionSynced` returned by
  the typed runtime surface.
- Rename/archive/delete saved sessions mutate through `TuiHostActions` and
  refresh the saved-session picker on success with the existing notices.
- Archive/delete reject the currently attached recorded session before any
  mutation.

## Frozen Failure And Lifecycle Semantics

- Candidate start, active-work, transcript-load, and preflight failures emit
  the existing `OperationRejected` and preserve the prior runtime/config/
  preload. Attachment rotation occurs only after successful installation.
- A typed history projection failure after installation emits the existing
  `OperationRejected`; it does not roll back the installed session or rotate a
  second time.
- Current rename failures emit `OperationRejected`; saved rename/archive/delete
  failures emit `SavedSessionActionFailed` with unchanged text.
- Candidate validation remains before prior-thread retirement. Replaced threads
  are reaped by the lifecycle owner's bounded five-second retry loop. This
  slice adds no new retry, timeout, or shutdown policy.
- Runtime/provider disconnect and action-channel disconnect behavior remains
  unchanged. The controller still owns action-channel termination and final
  active/Side thread shutdown.
- Restart behavior remains transcript-backed: resume/fork load the same saved
  session through `RuntimeSurfaceHostHandle`; failed restart candidates do not
  replace the current thread.
- Side entry/toggle/close, Side restrictions, final controller shutdown, and
  non-session actions remain in `app.rs`.

## Compatibility

- No CLI/TUI command, `UserAction`, `TuiEvent`, server JSONL, app-server, ACP,
  runtime surface, session-store, or transcript schema changes.
- Existing messages, attachment generations, projection order, and picker
  refresh behavior remain unchanged.
- No compatibility wrapper or second session state source is added.

## Test Strategy

1. Add a direct lifecycle-module test that calls the new action owner for
   `RenameCurrent` with no thread and proves the exact existing rejection with
   no thread/config/preload mutation.
2. Add owner-level archive/delete tests against a recorded current session and
   prove root-channel rejection, unchanged attachment/config/preload/thread,
   and a still-loadable, unarchived transcript.
3. Keep existing controller tests as behavioral coverage for new/fork/resume,
   rename, preflight preservation, attachments, and restart.
4. Run focused lifecycle/session tests, `cargo check`, full serial TUI, PTY,
   runtime/Windows validators and self-tests, formatter, and `git diff --check`.
5. Request independent review focused on attachment rotation, old-thread
   retirement, projection ordering, failure ownership, validator integrity, and
   public/persistence drift.

## Acceptance Criteria

1. The eight session action transactions have one implementation owner in
   `hosted_session_lifecycle.rs`; `app.rs` only maps commands.
2. The direct owner test is RED before the API exists and GREEN afterward.
3. Existing session/Side/restart behavior tests pass unchanged.
4. Contract validators remain meaningful and include a deletion-resistant
   production owner/dispatch anchor if source references move.
5. Full TUI and PTY suites pass after rebase and again on integrated local
   `main`.
6. Independent review has no unresolved Critical or Important finding.

## Implementation Evidence

- At audited base `9d621f1b9`, the direct lifecycle-owner test first failed
  because `HostedSessionAction` and `handle_hosted_session_action` did not
  exist. It passes after the owner was added.
- The eight production controller branches now contain only command mapping
  and one `handle_hosted_session_action` call each. The transaction bodies live
  in `hosted_session_lifecycle.rs`.
- Focused lifecycle, session, and picker tests plus the review-driven recorded
  current-session archive/delete guard regression and
  `cargo check -p orca-tui --tests --locked` pass.
- The runtime contract manifest now references both the lifecycle owner and
  controller dispatch. Path-specific anchors plus negative self-tests reject
  deletion of either production side while imports or test calls remain.
- Post-extraction source sizes are `app.rs` 9,266 lines and
  `hosted_session_lifecycle.rs` 853 lines.

## Non-Goals

- Extracting Side lifecycle or the full hosted controller loop.
- Changing session switch policy, active-work eligibility, persistence, title
  rules, or picker pagination.
- Changing retry budgets, attachment generation semantics, or history payloads.
- Reconciling cold legacy registry-only task records.

## Rollback

Revert the single semantic commit. No schema or data migration is involved.

## Residual Boundary

Side lifecycle, non-session controller actions, and the Goal/session lifecycle
module dependency require separate evidence. Cold registry reconciliation
remains independent migration work.
