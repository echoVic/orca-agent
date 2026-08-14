# TUI Convergence Slice 13: Edit Highlight State And Worker Ownership

## Status

Implemented and verified on `codex/tui-edit-highlight-owner`, based on local
`main` at `16f5fa8eb` after queued-submission ownership was integrated and
verified. Independent review's one Important shutdown-backlog finding was fixed
with an explicit fence and 1,000-job regression; no findings remain.

## Problem And Evidence

`AppState` currently stores seven coupled edit-highlight facts directly in
`types.rs`: the configured workspace root, syntax theme, terminal color level,
optional worker runtime, applied revision maps, and two test injection hooks.
Roughly 350 lines in the same general reducer own configuration, job admission,
workspace identity checks, stale-result validation, worker disconnect recovery,
derived-map pruning, and rendering lookup. `ui.rs` also reads the applied map
directly.

The worker has a separate lifecycle defect. `EditHighlightRuntime` receives a
`JoinHandle` from `spawn_worker` but discards it in `new_with_channels`. Dropping
or replacing the runtime disconnects channels, yet the old worker is no longer
owned or joined while it finishes bounded file parsing and highlighting.

This is process-local derived presentation state, not runtime-surface state.
The current architecture document already assigns AppState validation and
storage to this boundary; this slice makes that boundary explicit without
changing the underlying parser, syntax renderer, or result identity rules.

## User Value And Scope

Live edit rows must never inherit styles computed for another workspace,
theme, terminal color capability, message revision, reused tool id, retargeted
path, or changed diff. Reconfiguration must retire the previous worker and all
derived maps together. This slice:

- adds private `EditHighlightState` in `edit_highlight.rs` as the sole owner of
  configuration, worker runtime, applied revision maps, and test hooks;
- replaces the seven AppState fields with one aggregate;
- moves AppState edit-highlight policy methods beside that owner;
- exposes only command methods and immutable rendering/test queries;
- makes `EditHighlightRuntime` retain and join its worker after closing the job
  channel;
- migrates message lifecycle and renderer callers without changing output.

`edit_highlight_worker.rs` remains the owner of job coalescing, file reads,
syntax computation, pending job identities, and worker channel mechanics.
`diff_highlight.rs` remains the parser/rendering algorithm owner. AppState still
owns messages, message revisions, transcript-cache invalidation, and selection
invalidation.

## State And Transition Contract

`EditHighlightState` defaults to no workspace, One Half Dark, true color, no
runtime, no applied maps, and production worker/drain functions.

- Configure stores the new workspace/theme/color tuple, drops and joins any
  previous runtime, and clears every applied map. Pending work from the old
  configuration cannot later publish.
- Submit admits only a completed tool row with a nonempty structurally valid
  unified diff, an eligible destination file inside the canonical configured
  workspace, an exact normalized target match, a supported syntax highlighter,
  and a current message revision. Runtime creation or channel failure is silent
  and leaves no live runtime after send failure, preserving current UX.
- Poll drains every ready result without blocking. Disconnect retires the
  runtime. A result consumes only its exact pending job identity.
- Apply accepts only Ready output whose syntax tuple/revision, message index and
  revision, tool id, completed status, parsed diff, normalized destination, and
  current canonical target still match. AppState advances the message revision
  and invalidates its render cache before the owner stores styles under the new
  revision.
- Message touch, replacement, truncation, retention, clear, and tool-id reuse
  cancel or prune pending/applied state through owner commands exactly as today.
- Rendering receives an immutable applied-map view and still validates both
  message revision and tool id before using styles.

## Cancellation, Failure, And Recovery Semantics

- This state owns no external side effect. Jobs read at most the existing
  bounded file size and compute derived styles only.
- Runtime drop raises a shutdown fence, closes the job sender and result
  receiver, then joins the worker. The worker checks the fence before and during
  queue coalescing and before each computation. A worker already computing may
  finish that one bounded job, but cannot publish it or drain an unbounded queued
  backlog during retirement.
- Reconfiguration and channel disconnect may discard derived work silently, as
  they do today; the ordinary diff renderer remains the fallback.
- Malformed diffs, missing or retargeted files, symlink escapes, unsupported
  syntax, stale revisions, worker spawn failure, and worker disconnect all fail
  closed without transcript noise.
- State is intentionally process-local and rebuildable. Restart begins with the
  default owner and creates no persistence or replay obligation.

## Ownership And Compatibility

`edit_highlight.rs` is the unique owner of aggregate facts and AppState
highlight transitions. `types.rs` coordinates message lifecycle through those
commands but contains no highlight fact mutation. `ui.rs` receives only an
immutable map projection. Test queries may observe configuration, runtime,
pending counts, applied identities, and injected worker behavior; they do not
receive mutable owner state.

There is no CLI argument, TUI key flow, visible label, runtime event,
runtime-surface/server/JSONL protocol, history format, SQLite schema, or
persisted session change. `AppState` production call names remain crate-local
and source-compatible. No second map or temporary dual state is introduced.

## Acceptance

1. An owner-level RED test proves reconfiguration atomically retires a seeded
   runtime, clears seeded applied styles, and installs the new syntax tuple.
2. Worker lifecycle RED tests prove drop does not return until the worker has
   exited, closes the result path before join, and fences a queued backlog before
   coalescing can consume it.
3. AppState contains one `EditHighlightState` field and none of the seven old
   fields. Production code outside `edit_highlight.rs` has no direct fact
   mutation; renderer access is immutable.
4. Existing stale-result, malformed-diff, target/symlink, tool-id reuse,
   message-lifecycle, disconnect/respawn, rendering, and workspace
   configuration behavior remains green.
5. Focused gates pass:
   `cargo test -p orca-tui edit_highlight --lib --locked -- --test-threads=1`,
   `cargo test -p orca-tui stale_edit_highlight --lib --locked -- --test-threads=1`,
   and `cargo test -p orca-tui syntax_workspace_root --lib --locked -- --test-threads=1`.
6. Full TUI and root PTY suites, both Node validator suites, direct contract
   validation, formatting, diff checks, and an obsolete-field search pass.
7. Independent review finds no ownership leak, unjoined worker, stale style
   publication, renderer regression, or missing failure-path test.

## Migration, Deletion, And Rollback

The aggregate, worker join ownership, method move, field deletion, caller/test
migration, validator baseline refresh, and roadmap update land in one semantic
commit. The old fields and method definitions are deleted in the same slice.
One commit revert restores the previous process-local layout; no data migration
or recovery step exists.

## Spec Self-Review

Normal admission, reconfiguration, worker shutdown, channel failure, stale
results, malformed/retargeted input, message lifecycle, restart, and rendering
fallback are explicit. The worker, aggregate, AppState message model, diff
algorithm, and renderer boundaries remain distinct. Acceptance is behavioral,
with source searches used only to verify the deletion/ownership boundary.
