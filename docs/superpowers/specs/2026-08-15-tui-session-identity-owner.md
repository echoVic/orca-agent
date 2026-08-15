# TUI Convergence Slice 16: Session Identity Projection Ownership

## Status

Implemented on `codex/tui-session-identity-owner`, based on clean local `main`
at `ac5e06880` after the Goal projection ownership slice was integrated and
verified. RED/GREEN evidence and final locked gates pass. Independent review
found no Critical or Important findings after Side reset-order, rejected-reset,
and retired-relay interaction fixes; rebase,
local-main integration, and owned-worktree cleanup remain pending. This slice
does not complete the broader TUI convergence or authorize a release.

## Problem And Evidence

The TUI currently has six production paths that can rewrite one displayed
session identity. `SurfaceProjectionSynced` copies a snapshot-derived id and
title into `AppState`, while `NewSessionStarted`, `SessionProjectionReset`,
`SessionIdentityUpdated`, `SessionRenamed`, and `SessionForked` also assign the
same public mutable fields. Rename and fork events mix committed facts with
presentation. Reset events carry caller-authored placeholder titles such as
`Restored conversation` before a later snapshot replaces them.

The snapshot conversion also labels every `SurfaceThreadId` as `session_id`.
That is incorrect for `EphemeralNonCataloguedOneShot` and
`EphemeralAttached`: a runtime thread exists, but no recorded session can be
resumed. The runtime snapshot already carries the required distinction in
`SurfaceThreadSnapshot.persistence`, and recorded runtime thread ids are the
durable session ids.

This is an architecture and boundary defect. A stale granular event can
overwrite a newer title without a cursor, an implicit identity change can reset
unrelated snapshot owners, and an ephemeral thread can be presented as a saved
session. The runtime surface snapshot is already the authoritative identity
fact, but the TUI has not made that ownership explicit.

## User Value And Scope

`/status`, session-picker actions, resume hints, new/resume/fork transitions,
side-conversation switching, and rename feedback must describe the currently
attached runtime thread without inventing resumability. This slice:

- adds one private `SurfaceSessionProjectionState` as the sole AppState owner
  of the optional recorded session id, authoritative title, accepted cursor,
  and last presentation cursor;
- derives `session_id: Option<String>` from snapshot persistence instead of
  treating every surface thread id as a recorded session;
- makes accepted surface snapshots and explicit reset snapshots the only
  production updates for session identity facts;
- requires an explicit reset before a different thread or incarnation can be
  accepted, making identity the gate for the rest of a projection envelope;
- turns rename and fork acknowledgements into atomic presentation directives
  on final authoritative snapshots;
- removes `SessionIdentityUpdated`, `SessionRenamed`, and `SessionForked`, and
  removes identity payloads from the new-session control signal;
- replaces the public mutable AppState fields with immutable
  `current_session_id()` and `current_session_title()` queries;
- removes the app-loop `active_session_id` shadow and derives the exit resume
  hint from the AppState owner;
- preflights the first authoritative snapshot before installing a newly
  started new/resumed/forked runtime thread, so a projection failure cannot
  silently switch runtime ownership while leaving the old identity visible.

Workflow-task projection, foreground/recoverable operation projection,
attachment identity, side-parent metadata, saved-session catalog contents,
runtime thread/session types, and renderer orchestration remain outside this
slice. They do not receive a compatibility cache or a second identity source.

## State And Transition Contract

`SurfaceSessionProjectionState` starts with no accepted cursor, no recorded
session id, no title, and no presented cursor.

- `SurfaceProjectionState.session_id` is `Some(surface thread id)` only for
  `ThreadPersistence::RecordedCatalogued`; both ephemeral persistence variants
  project `None`. The title always comes from `snapshot.thread.title`.
- The first ordinary snapshot is accepted. Later ordinary snapshots are
  comparable only when thread id and incarnation match the accepted cursor.
  A different identity or incarnation is rejected before metrics, Goal,
  workflow, or operation fields are applied.
- Within one identity, a higher `next_seq` replaces the optional session id and
  title. A lower sequence is rejected. Equal cursor plus equal identity is
  idempotent; equal cursor plus different identity is rejected as an invariant
  violation.
- `SessionProjectionReset(Box<SurfaceProjectionState>)` first validates the
  reset envelope, then clears all session-scoped AppState projection and
  transient UI state, resets the session owner, and applies its authoritative
  snapshot. A rejected reset leaves the previous state intact. It is the only
  transition that admits a different thread/incarnation into a non-empty owner.
- `NewSessionStarted` remains a payload-free control signal for changing the
  local history mode to `Record` and clearing the composer. It cannot change
  session identity.
- A session presentation directive is processed atomically with its accepted
  snapshot. `Renamed` requires a recorded session and renders
  `Renamed conversation to <authoritative title>.`; `Forked` requires a
  recorded session and renders `Forked conversation as <authoritative title>.`.
  One exact cursor can be presented at most once.
- Startup hydration may accept the first ordinary snapshot without a reset.
  New, resume, fork, and side-thread switches require a reset snapshot before
  history or later ordinary snapshots are accepted.
- Renderers, slash commands, session-picker actions, and exit policy borrow the
  current identity through immutable AppState queries.

## Normal Lifecycle Semantics

- Lazy first submit starts one runtime thread, publishes `MentionRuntimeReady`
  for mention actions, then publishes the first authoritative projection. A
  disabled-history thread owns a title but has no current session id.
- `/new` starts and snapshots a recorded thread before replacing the prior
  controller thread. After installation it rotates the attachment, publishes
  the reset snapshot, publishes runtime readiness, and emits the payload-free
  new-session control signal.
- Current-session fork and saved-session fork snapshot the new recorded thread
  before installation, publish a reset snapshot, hydrate copied history, then
  publish a final snapshot carrying `Forked` presentation.
- Resume snapshots the resumed recorded thread before installation, publishes
  a reset snapshot, and hydrates history with a normal final snapshot.
- Current-session rename commits the surface metadata mutation, persists the
  catalog title, reads a fresh snapshot at or beyond the committed cursor, and
  returns that snapshot with `Renamed` presentation. The TUI never publishes
  the requested title directly.
- Switching into or out of Side Conversation reads the selected thread's
  current projection batch before changing attachment ownership and uses it as
  the reset identity. Returning to the parent activates the parent attachment,
  sends reset and inherited history in root-channel order, then releases
  queued parent interactions. Every parent-to-Side reactivation rotates the
  Side attachment generation before activation, fencing late events from its
  retired relay before the reset/history batch. Interactions queued by the
  retired generation retain their source attachment and are discarded during
  retirement rather than replayed as parent prompts. Ephemeral Side threads
  therefore keep `current_session_id() == None` while their title remains
  visible.

## Cancellation, Failure, Retry, Disconnect, And Restart Semantics

- The session owner creates no worker, task, connection, cancellation token, or
  durable write. Runtime thread shutdown/reaping, attachment rotation, and
  cancellation remain owned by the controller and runtime.
- A newly started new/resumed/forked thread is not installed until a fresh
  snapshot is readable and its projected optional recorded id agrees with the
  handle's `session_id()`. A failed preflight shuts down/reaps that uninstalled
  thread, leaves the current thread and AppState identity unchanged, and emits
  an operation rejection.
- Side-thread startup and Side return/toggle use the same preflighted projection
  batch before installing or activating the target. A failed read leaves the
  current runtime and attachment selected; an uninstalled Side handle is
  reaped.
- Rename returns success only after both runtime and catalog commits and an
  authoritative post-commit snapshot whose cursor covers the runtime metadata
  commit. If the catalog write fails, the existing compensation semantics stay
  intact. If the final snapshot cannot prove the commit, the error states that
  rename committed but TUI projection failed; no local title is fabricated.
- If a later concurrent metadata commit wins before the final rename read, the
  acknowledgement uses that final authoritative title.
- A reset snapshot send can fail only after the TUI receiver disconnects. The
  already installed controller thread remains owned until controller shutdown;
  no fallback identity event is sent. Attachment generation fencing continues
  to reject events from the previous thread.
- Cursor gaps, stale cursors, contradictory equal-cursor payloads, and ordinary
  snapshots from another identity cannot partially mutate any snapshot owner.
- A malformed reset presentation cannot clear the previous transcript or
  identity; reset admission is transactional at the reducer boundary.
- Subscription disconnect retains the last accepted identity. Reattach reads a
  fresh snapshot. Process restart starts with an empty process-local owner and
  accepts startup hydration.
- Session switch remains rejected while active work exists. The slice changes
  no operation cancellation, wait, recovery, or terminal semantics.

## Ownership And Compatibility

The runtime surface reducer remains the authoritative runtime-thread fact
owner. `SurfaceProjectionState` is the committed snapshot envelope.
`SurfaceSessionProjectionState` is the unique process-local identity owner.
`AppState` coordinates explicit reset of all session-scoped presentation state
but contains no mutable identity fact fields.

There is no CLI argument, slash-command syntax, key binding, visible wording,
server/JSONL or ACP protocol, runtime surface event, SQLite schema, transcript
format, or saved-session catalog format change. Removing three `TuiEvent`
variants, changing `SessionProjectionReset` and `NewSessionStarted` payload
shapes, making projection `session_id` optional, and replacing public mutable
AppState fields are Rust source changes. Workspace callers migrate in the same
commit; the immutable getter path is the supported replacement. This internal
`orca-tui` 0.1 migration is accepted because keeping the old payloads or fields
would retain competing fact sources. One semantic commit is the rollback
boundary.

## Acceptance

1. RED behavior proves that a disabled-history snapshot has no recorded session
   id, an older snapshot cannot restore an old title, a contradictory equal
   cursor is rejected, and a different thread is accepted only after reset.
2. AppState has one `SurfaceSessionProjectionState` and no public mutable
   `current_session_id` or `current_session_title` field. Production readers use
   immutable queries, and no app-loop identity shadow remains.
3. `SurfaceProjectionSynced` and `SessionProjectionReset` are the only
   production identity fact updates. Searches find no `SessionIdentityUpdated`,
   `SessionRenamed`, or `SessionForked`.
4. New, resume, current fork, saved fork, and Side switches publish reset
   snapshots whose optional session id and title equal the selected runtime
   snapshot. New-session composer reset and history-mode behavior remain.
5. Rename and fork publish one presentation for one accepted cursor, use the
   final authoritative title, and do not precede it with a granular identity
   event.
6. A pre-install projection failure leaves the previous runtime thread and
   AppState identity selected and retires the uninstalled thread.
7. Startup resume remains silent except for its existing history label;
   process restart and attachment rotation accept the correct identity without
   stale-thread mutation.
8. Focused owner, lifecycle, rename, fork, resume, disabled-history,
   side-switch identity, status, session-picker, attachment-routing, and
   exit-policy tests pass.
9. Locked TUI check, full serial TUI library suite, root PTY contract,
   runtime-surface validator and self-test, Windows validator self-test,
   formatting, diff integrity, and obsolete-path searches pass.
10. Independent review finds no fabricated resumability, stale title overwrite,
    implicit cross-thread acceptance, reset ordering regression (including
    queued Side-parent interactions, a reactivated Side relay, or retired-Side
    interactions replayed as parent prompts), rename commit ambiguity, duplicate
    presentation, public compatibility leak, or missing restart/failure coverage.

## Migration, Deletion, And Rollback

The migration order is RED identity tests, optional identity envelope and
cursor owner, explicit reset application, pre-install projection proof,
rename/fork presentation migration, reader migration, old-event/field/shadow
deletion, existing lifecycle test migration, docs/validator refresh, full
verification, independent review, rebase, and main-only integration.

No temporary facade or compatibility event remains. A one-commit revert
restores the prior public fields and granular events. There is no durable data
migration, external protocol coordination, push, tag, GitHub Release, or npm
publication in this architecture-only slice.

## Spec Self-Review

There are no placeholders. Recorded and ephemeral identity, ordinary and reset
projection, rename/fork presentation, new/resume/fork/Side lifecycle, failure,
retry, disconnect, restart, cancellation ownership, compatibility, deletion,
and rollback are explicit. Each behavior has executable acceptance evidence,
and the slice leaves workflow and operation ownership as separate later
boundaries rather than adding a second identity cache.
