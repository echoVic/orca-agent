# TUI Convergence Slice 19: Workflow Panel State Ownership

## Status

Proposed on `codex/tui-workflow-panel-state-owner`, based on clean local
`main` at `c6b23266e`. This is an internal TUI state-ownership slice. It does
not complete the broader runtime workflow-task reconciliation or authorize a
release.

## Problem And Evidence

`workflow_panel.rs` already owns workflow-panel navigation, task refresh
routing, background-task reveal behavior, and background approval opening.
Its state is still declared in `types.rs` as public `WorkflowPanelState`
fields, and production callers read or write `state.workflow_panel.tasks` and
`state.workflow_panel.selected` directly from rendering, action, reducer, and
test modules.

The public fields let a caller replace the task list or select an out-of-range
index without using the panel transition that preserves a selected task id,
opens the panel for a newly backgrounded task, or returns to the conversation
after foregrounding. The state machine and its data therefore have different
owners. The completed runtime-task audit remains a separate boundary: recorded
TUI workflow commands already project `SurfaceSnapshot.tasks`, but non-recorded
runtime threads still expose live `TaskRegistry` summaries and there is no
production `TaskPatch::Reconciled` producer. This slice must not hide that
runtime gap with a TUI cache or alter task-source selection.

Classification: architecture boundary. The affected state is process-local
presentation state; the runtime surface, `TaskRegistry`, and persistence remain
authoritative for task facts.

## User Value And Scope

The visible workflow and approval panels must retain their current selection,
ordering, background reveal, and foreground-return behavior while presentation
code cannot bypass the state transition that maintains those invariants. This
slice:

- moves `WorkflowPanelState` into `workflow_panel.rs` and makes its task list
  and selected index private;
- makes `workflow_panel.rs` the only production module that replaces tasks,
  changes selection, or looks up the selected task;
- exposes immutable AppState queries for renderers and action handlers;
- keeps all task-list updates routed through the existing
  `apply_workflow_tasks_update` behavior, including sort order, selection
  retention, background reveal, approval reveal, and foreground return;
- migrates test setup to narrowly scoped test-only helpers instead of public
  field mutation; and
- records the unchanged runtime reconciliation deferral in the roadmap.

It does not change `TuiEvent::WorkflowTasksUpdated`,
`TuiEvent::WorkflowTaskUpdated`, runtime surface task patches, runtime task
control, workflow execution, server or JSONL protocol, task persistence,
history format, terminal layout, or workflow panel commands and keys.

## State And Transition Contract

`WorkflowPanelState` owns exactly two presentation values:

- the task vector as last supplied by the accepted TUI task projection; and
- the selected list index.

The state has immutable `tasks()`, `selected()`, and `selected_task()` queries.
Its transitions are private to `workflow_panel.rs`:

- `replace_tasks` installs the panel-sorted list and clamps the selected index;
- `select_previous` and `select_next` keep selection in bounds, including an
  empty list where it remains zero;
- `select_index` clamps a requested index; and
- `reset_for_session` restores the default empty panel for an accepted session reset.

`AppState::apply_workflow_tasks_update` remains the only production full-list
transition. Before replacement it reads the currently selected id and whether
that selection was a backgrounded main session. After replacement it performs
the existing user-facing routing: newly backgrounded running work or a newly
backgrounded approval opens the workflow panel, selection follows the matching
task, and foregrounding the selected main session returns the user to the
conversation panel. A single-task event first builds an updated full list from
the immutable query and then uses the same full-list transition.

## Lifecycle, Failure, And Restart Semantics

- Task projections are still accepted or rejected by the existing runtime
  surface and attachment machinery before they reach this state. This slice
  creates no cursor, revision, retry, timer, worker, cancellation token, or
  durable record.
- A session reset clears the panel state together with the other session-local
  presentation state. The following accepted task projection is the only path
  that repopulates it.
- Empty, failed, stale, disconnected, restarted, and non-recorded runtime task
  semantics are unchanged. The panel retains the last accepted process-local
  task view until its existing session reset or a later event; it does not
  synthesize a task list from `TaskRegistry`.
- Task stop, foreground, and background approval actions still call the
  runtime-surface facade and then apply the returned `WorkflowTasksUpdated`
  list. Failures keep their existing error and notice behavior and do not
  partially mutate panel state.

## Ownership And Compatibility

The runtime surface and legacy compatibility event routes remain task-fact
sources exactly as before. `WorkflowPanelState` is a private process-local
presentation owner; it is not another runtime reducer. `AppState` owns panel
mode, status, approval dialog, notification queue, and output-suppression
effects that the existing panel transition changes.

`WorkflowPanelState` and `AppState::workflow_panel` become crate-private
implementation details. Workspace production callers use AppState queries and
actions. This is an intentional internal Rust source compatibility change;
there is no CLI, TUI workflow, server/JSONL, ACP, persistence, or history
compatibility change. No mutable compatibility facade is retained because it
would preserve the bypassed transition path.

## Acceptance

1. A RED test proves the state owner replaces and sorts tasks, keeps selection
   in bounds, preserves selected task identity across a refresh, and resets to
   an empty selection. It fails before the owner API exists.
2. `WorkflowPanelState` lives only in `workflow_panel.rs`; its task vector and
   selected index are private. Production reads use immutable queries and no
   production code assigns `workflow_panel.tasks` or
   `workflow_panel.selected`.
3. Existing observable workflow behavior remains unchanged: panel navigation,
   task sorting, selected-id retention, newly backgrounded task/approval reveal,
   selected foreground return, task stop, foreground, and background approval
   actions all use the same state transition.
4. Session reset clears the owner and the next accepted projection can hydrate
   it. A `WorkflowTaskUpdated` event continues to merge one task through the
   full-list transition.
5. The runtime reconciliation audit remains explicit: this slice adds no
   `TaskPatch::Reconciled` producer, no direct `TaskRegistry` read in TUI
   presentation code, and no durable task cache.
6. Focused workflow-panel, projection, and task-control tests pass, followed
   by locked TUI compilation, the full serial TUI library suite, root PTY
   contract, both validator self-tests, runtime-surface validator, formatting,
   diff integrity, and obsolete direct-field searches.
7. Independent review finds no direct production mutation, selection-boundary
   regression, stale task-source fallback, reset leakage, runtime/protocol
   change, or missing action coverage.

## Migration, Deletion, And Rollback

The migration order is a RED owner test, private state and immutable queries,
production reader/writer migration, test-fixture migration, reset and
projection verification, documentation, full gates, review, rebase, and
main-only integration. The old public fields and every production direct
assignment are removed in the same semantic commit. There is no compatibility
adapter, shadow task store, persistence migration, or worker. Reverting the one
commit restores the prior internal representation without data migration. No
push, tag, GitHub Release, or npm publication is authorized.

## Spec Self-Review

The scope is one independently testable presentation owner. Normal refresh,
empty list, task selection, backgrounding, foregrounding, approval, reset,
stale/disconnected/restart behavior, runtime ownership, compatibility,
deletion, verification, and rollback are defined. No runtime task fact is
duplicated or hidden, and no placeholders remain.
