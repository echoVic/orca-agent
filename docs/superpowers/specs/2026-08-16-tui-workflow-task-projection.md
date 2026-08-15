# TUI Convergence Slice 21: Workflow Task Projection Ownership

## Status

Implemented on `codex/tui-workflow-task-projection`, based on clean local
`main` at `3e2006196`. Focused and full gates pass, and independent re-review
finds no remaining Critical or Important issue. This is an internal TUI
projection and runtime-facade slice. It does not change the runtime surface
schema, persisted event format, CLI, server/JSONL, or ACP protocol, complete
cold legacy-task reconciliation, or authorize a release.

## Problem And Evidence

The typed runtime already commits live task, workflow, and subagent facts into
`SurfaceSnapshot` for recorded and ephemeral TUI threads. Every relevant typed
batch also ends in `TuiEvent::SurfaceProjectionSynced`, whose
`SurfaceProjectionState.workflow_tasks` is applied through the workflow panel's
single full-list transition. The TUI nevertheless retains two granular fact
events, `WorkflowTasksUpdated` and `WorkflowTaskUpdated`.

The full-list event is emitted before the final snapshot for task/workflow
batches, emitted again by background presentation monitors, emitted after
startup already sent the snapshot, and returned from stop, foreground, and
background-approval actions. The single-task variant has no production sender
but remains reducible by merging into the last local list. In addition,
`surface_client::foreground_task` bypasses the typed surface for ephemeral
threads, and the workflow stop fallback merges `RuntimeSurfaceThreadHandle::
task_summaries()` into snapshot summaries.

These paths let an unversioned registry list or granular event replace task
facts independently of the accepted surface cursor. A delayed list can
overwrite a newer snapshot, while registry-only recovered approvals can appear
actionable without a surface interaction or control fence. The existing
`recovered_registry_only_background_approval_is_not_advertised` regression
already establishes the correct fail-closed behavior.

Classification: architecture boundary. The runtime surface is the unique live
task-fact source; `WorkflowPanelState` is only the process-local presentation
owner.

## User Value And Scope

The task and approval panels must show one cursor-consistent view after task
creation, backgrounding, progress, approval, foregrounding, stop, terminal
completion, Side reentry, and restart. This slice:

- makes accepted `SurfaceProjectionSynced` and `SessionProjectionReset`
  payloads the only production inputs that replace workflow-panel task facts;
- deletes `TuiEvent::WorkflowTasksUpdated` and
  `TuiEvent::WorkflowTaskUpdated`, their reducer paths, and every sender;
- removes the extra full-list projection from typed task/workflow batches and
  typed history hydration because the final snapshot already contains it;
- changes stop, foreground, recovered-approval, and approval-resolution
  facades to return or publish one post-commit `SurfaceProjectionState`;
- makes ephemeral foreground control use the same surface task fence and
  operation hydration path as recorded threads;
- removes the workflow-stop merge with live `TaskRegistry` summaries; and
- preserves panel sorting, selected-id retention, background/approval reveal,
  foreground return, notices, and output handoff.

It does not add a `TaskPatch::Reconciled` producer. Live typed operations
already publish native task, workflow, and subagent patches. Reconciliation of
pre-surface legacy registry records is a separate cold migration boundary and
must not be approximated by exposing unfenced registry rows in the TUI.

## State And Transition Contract

`SurfaceProjectionState.workflow_tasks` is derived only from the reducer's
`SurfaceSnapshot.tasks`, `workflows`, `subagents`, and interactions. A private
task projection owner admits the cursor and full task payload before any task
replacement. A lower, cross-thread, cross-incarnation, or contradictory
equal-cursor payload cannot mutate the panel. An accepted payload calls the
existing `apply_workflow_tasks_update` transition exactly once, retaining all
current presentation effects.

`TuiSurfaceProjection::project_typed_batch` may continue emitting lifecycle,
stream, notice, and terminal presentation events, but a task/workflow/subagent
batch appends exactly one final `SurfaceProjectionSynced` event and no granular
task-list event. Background monitors publish the accepted projection snapshot
for a changed task and derive any one-time approval notice from that same
projection. They do not construct an independent task fact.

Task actions execute the existing typed mutation, drain any bound operation as
required, read the post-commit surface snapshot, and return its projection.
The caller publishes that projection before the existing success notice. A
snapshot-read or mutation failure publishes the existing error, no success
notice, and no fabricated task list.

## Lifecycle, Failure, And Restart Semantics

- Normal live commits: the final typed snapshot atomically replaces the visible
  task list after earlier lifecycle events in the same projected batch.
- Background and foreground: ownership remains in the runtime task and
  background fences. Foregrounding an ephemeral Side task uses the same typed
  control, hydration watermark, presentation retirement, and join ownership as
  a recorded task.
- Approval and rejection: a committed background approval response is followed
  by its post-commit snapshot and notice. A denied, stale, missing, or
  unauthorized response preserves the last accepted panel and emits the
  existing error.
- Stop and cancel: stop uses typed task/workflow control. Cancellation, terminal
  waiting, and background presentation retirement remain owned by the existing
  controller and runtime actor; this slice creates no worker or cancellation
  token.
- Timeout and retry: existing bounded attach/read retries remain unchanged. No
  registry fallback is used after timeout or surface unavailability.
- Disconnect: the panel keeps the last accepted process-local snapshot until a
  later accepted projection or session reset. It does not poll the registry.
- Restart: recorded sessions hydrate task facts from the recovered surface
  snapshot. Registry-only legacy rows that lack surface facts remain hidden and
  non-actionable.
- Side reentry: the fresh attachment reset and background presentation rebind
  continue to fence stale events; the replacement monitor publishes only
  projection snapshots bound to the active attachment.

## Ownership And Compatibility

The runtime actor and surface reducer own task, workflow, subagent, interaction,
and cursor facts. `TuiSurfaceProjection` is the typed adapter from a committed
batch to presentation events plus its final snapshot. `SurfaceProjectionState`
is the accepted snapshot envelope. `WorkflowPanelState` owns only sorting,
selection, reveal, and return-to-conversation effects.

The two deleted `TuiEvent` variants and changed crate-private facade return
types are internal Rust source changes. There is no CLI argument, slash command,
key binding, rendered workflow, server/JSONL event, ACP event, runtime surface
event, stored surface schema, session history, or task-registry persistence
change. No compatibility event or shadow list remains. The public
`RuntimeSurfaceThreadHandle::task_summaries` API is outside the TUI boundary and
is not removed by this slice.

## Acceptance

1. A RED projection test proves a task patch produces lifecycle presentation as
   applicable and ends in exactly one `SurfaceProjectionSynced`, with the task
   present in `workflow_tasks`, and no granular task event.
2. A RED ephemeral integration test backgrounds a Side/main task, foregrounds
   it through the typed surface path, and proves task visibility, output
   handoff, terminal delivery, and non-backgrounded state all arrive through
   accepted projection snapshots.
3. Startup/restart tests prove one snapshot hydrates completed workflow tasks
   without a following granular list. Registry-only recovered approvals remain
   hidden and non-actionable.
4. Stop, foreground, recovered approval, and approval response paths publish a
   post-commit `SurfaceProjectionSynced` before their success notice. Failed
   mutations and snapshot reads preserve the last panel and publish no success
   projection.
5. `WorkflowTasksUpdated` and `WorkflowTaskUpdated` have no enum variant,
   sender, reducer branch, test fixture, or compatibility shim. TUI production
   code has no `task_summaries()` call or registry/snapshot merge.
6. Workflow panel sorting, selected-id retention, background/approval reveal,
   foreground return, Side background reentry, workflow progress, and terminal
   notifications remain behaviorally covered.
7. Locked runtime and TUI focused tests, the full serial TUI library suite,
   runtime surface tests affected by typed task control, root PTY contract,
   validator self-tests, runtime-surface validation, formatting, diff integrity,
   and obsolete-path searches pass.
8. Independent review finds no second task source, lost rich workflow/subagent
   projection, stale task overwrite, unfenced ephemeral foregrounding, hidden
   registry fallback, notice-order regression, or external compatibility
   change.

## Verification Commands

```bash
cargo test -p orca-tui surface_projection::tests::task_patch_projects_one_authoritative_snapshot --lib --locked -- --exact --test-threads=1
cargo test -p orca-tui types::tests::workflow_task_projection_fences_contradictory_equal_cursor --lib --locked -- --exact --test-threads=1
cargo test -p orca-tui app::tests::hosted_side_background_task_foreground_uses_surface_projection --lib --locked -- --exact --test-threads=1
cargo test -p orca-tui app::tests::resumed_registry_only_approval_is_not_advertised_as_actionable --lib --locked -- --exact --test-threads=1
cargo test -p orca-tui workflow --lib --locked -- --test-threads=1
cargo test -p orca-tui backgrounded --lib --locked -- --test-threads=1
cargo test -p orca-tui side_ --lib --locked -- --test-threads=1
cargo test -p orca-runtime runtime_surface --lib --locked -- --test-threads=1
cargo check -p orca-tui --tests --locked
cargo test -p orca-tui --lib --locked -- --test-threads=1
cargo test --test tui_pty_contract --locked -- --test-threads=1
node --test scripts/test-validate-runtime-surface-contract.mjs
node --test scripts/test-validate-windows-platform-boundaries.mjs
node scripts/validate-runtime-surface-contract.mjs
node scripts/validate-windows-platform-boundaries.mjs
cargo fmt --all -- --check
git diff --check
rg -n 'WorkflowTasksUpdated|WorkflowTaskUpdated' crates/orca-tui/src --glob '*.rs'
rg -n 'task_summaries\(' crates/orca-tui/src --glob '*.rs'
```

The first obsolete-event search must be empty. The second may find only a
projection helper whose receiver is `TuiSurfaceProjection`; it must not find
`RuntimeSurfaceThreadHandle::task_summaries` or a live registry fallback.

## Migration, Deletion, And Rollback

Migration order is RED projection and ephemeral-control tests, typed facade
return migration, monitor/startup migration, granular-event deletion, focused
behavior verification, documentation, full gates, independent review, rebase,
and main-only integration. The obsolete variants, reducers, senders, and
registry merge are deleted in the same semantic commit. Reverting that commit
restores the internal adapter rails without data migration. No push, tag,
GitHub Release, npm publication, or remote cleanup is authorized.

## Spec Self-Review

The slice defines normal, background, foreground, approval, rejection, stop,
cancel, timeout, retry, disconnect, restart, Side reentry, ownership,
compatibility, verification, deletion, and rollback semantics. It contains no
placeholder, does not invent a second task store or long-lived adapter, and can
be implemented, reviewed, committed, rebased, and reverted independently.
