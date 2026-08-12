# TUI Convergence Slice 8: Workflow Panel And Background Approval Routing Extraction

## Status

Proposed for `codex/tui-convergence`, based on `main` at `c1df2c256`.

## Problem And Evidence

`crates/orca-tui/src/types.rs` is 9,442 lines: the `AppState` struct plus two
large impl blocks mix transcript/search/scroll state with the workflow-panel
state machine. The workflow panel and background-approval routing category is
a coherent ownership unit spread over both impl blocks:

- Block 1 (types.rs:2252-2330): `show_workflows`, `show_agents`,
  `select_previous_workflow_task`, `select_next_workflow_task`,
  `open_selected_background_approval_dialog`, `push_pending_workflow_notification`,
  `show_conversation`.
- Block 2 (types.rs:3676-3754): `apply_workflow_tasks_update` (background
  task routing: reveal panel on backgrounded task/approval, re-anchor
  selection, demote foregrounded main session).
- Exclusive helpers (types.rs:285, 3798-3830):
  `push_pending_workflow_notification_unique`, `sort_workflow_tasks_for_panel`,
  `is_backgrounded_running_main_session`, `is_backgrounded_approval_main_session`,
  `is_foregrounded_running_main_session`.

Behavioral tests already cover the category end-to-end (app.rs tests:
`workflows_panel_keys_move_selected_task`,
`workflows_panel_enter_opens_selected_background_approval`,
`background_approval_resolution_sends_request_scoped_action`,
`background_approval_action_denial_stops_task_and_refreshes_tasks`, and the
surface-boundary typed-approval tests).

Classification: architecture boundary (module ownership), no behavior change.

## Scope

Create `crates/orca-tui/src/workflow_panel.rs` (registered in lib.rs) that
owns:

- `impl AppState` with the eight methods above. The two currently private
  methods (`push_pending_workflow_notification`, `apply_workflow_tasks_update`)
  become `pub(crate)` because Rust inherent-method privacy is scoped to the
  module containing the impl block; their callers stay in `types.rs`
  (`update` at 3153/3161/3170, `apply_surface_projection_state` at 1846).
  All `pub` methods keep their exact visibility and signatures — no public
  API change.
- The five helper free functions, `pub(crate)`; `types.rs` imports back the
  two it still uses (`push_pending_workflow_notification_unique` for
  `PendingWorkflowNotificationQueue::push_unique`, `sort_workflow_tasks_for_panel`
  for `apply_surface_projection_state`).

No logic edits beyond the relocation and the two visibility upgrades.

## Non-Goals

- No move of the `update` reducer, `ApprovalDialog`/`PanelMode`/`AppStatus`
  type definitions, `approval_actions.rs`, or the surface-client approval
  path.
- No CLI/TUI-flow/server/JSONL/persistence changes.
- No source-line-count assertions; the existing tests are the oracle.

## Ownership

`workflow_panel.rs` owns workflow-panel navigation, background-approval
dialog opening, and background-task reveal routing. `types.rs` owns the
state fields and the `update` reducer that dispatches into this category.
`surface_actions.rs`/`workflow_panel_actions.rs` keep owning the typed
interaction dispatch into the panel.

## Normal / Failure Semantics

Unchanged: the methods are pure state transitions; `open_selected_background_approval_dialog`
returns `false` without side effects when the selection is not a main-session
task in `ApprovalRequired` status or has no pending tool call.

## Acceptance

1. The eight methods live in `workflow_panel.rs` (impl AppState), the five
   helpers live there as `pub(crate)` free functions, types.rs imports what
   it still uses; compile clean.
2. Behavioral oracle unchanged and green:
   - `cargo test -p orca-tui --lib --locked` (1034 tests)
   - `cargo test -p orca-tui --test tui_pty_contract --locked -- --test-threads=1` (6 tests)
3. `cargo fmt --all -- --check` and `git diff --check` clean.
4. Both surface-contract validators green (baseline-maintenance step from
   the CI fix):
   - `node --test scripts/test-validate-runtime-surface-contract.mjs`
   - `node --test scripts/test-validate-windows-platform-boundaries.mjs`
5. Diff review: relocation only; the only semantic edits are the two
   `fn` → `pub(crate) fn` upgrades and import adjustments.

## Rollback

Single revertible commit; no persisted state.

## Migration

No temporary state; the old paths (methods in types.rs) are removed in the
same commit, not kept as wrappers.
