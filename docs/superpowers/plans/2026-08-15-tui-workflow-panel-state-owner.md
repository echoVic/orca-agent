# TUI Workflow Panel State Ownership Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make workflow-panel task rows and selection one private, invariant-preserving TUI presentation owner while retaining every current task projection and user interaction.

**Architecture:** `WorkflowPanelState` moves to `workflow_panel.rs` with private fields and a small state API. `AppState` retains the existing cross-state presentation effects, while renderers and actions consume immutable AppState queries and all task changes go through `apply_workflow_tasks_update`.

**Tech Stack:** Rust, Ratatui TUI state, `orca-runtime` typed surface task projections, Cargo behavior tests, Node runtime-surface contract validator.

---

### Task 1: Prove Workflow Panel Owner Invariants

**Files:**
- Modify: `crates/orca-tui/src/workflow_panel.rs`

- [x] **Step 1: Add a focused failing owner test**

Add a `workflow_panel.rs` unit test that uses the intended state API and makes
the selection behavior observable:

```rust
#[test]
fn workflow_panel_owner_sorts_preserves_selection_and_clears() {
    let mut panel = WorkflowPanelState::default();
    panel.replace_tasks(vec![workflow_task("later", 20), workflow_task("first", 10)]);
    panel.select_index(0);
    assert_eq!(panel.selected_task().map(|task| task.id.as_str()), Some("later"));

    panel.replace_tasks(vec![workflow_task("later", 30), workflow_task("new", 40)]);
    assert_eq!(panel.selected_task().map(|task| task.id.as_str()), Some("later"));

    panel.clear();
    assert!(panel.tasks().is_empty());
    assert_eq!(panel.selected(), 0);
}
```

Define a local `workflow_task(id, created_at_ms)` test helper in this module's
test block by copying the complete `BackgroundTaskSummary` field shape from
`types.rs`'s existing `workflow_task_summary` helper, setting
`created_at_ms`, `started_at_ms`, and `last_activity_at_ms` to the supplied
timestamp. Use the existing panel sorting criteria. The test must assert
selected identity rather than a source line or raw field shape.

- [x] **Step 2: Run the RED test**

Run:

```bash
cargo test -p orca-tui workflow_panel::tests::workflow_panel_owner_sorts_preserves_selection_and_clears --lib --locked -- --exact --test-threads=1
```

Expected: compilation fails because `WorkflowPanelState` has no private-owner
transition/query API.

### Task 2: Create The Private Panel State API

**Files:**
- Modify: `crates/orca-tui/src/types.rs:736-741, 865, 1022, 1345-1351, 1442, 2140-2148`
- Modify: `crates/orca-tui/src/workflow_panel.rs`

- [x] **Step 1: Move the type and make fields private**

Move `WorkflowPanelState` from `types.rs` into `workflow_panel.rs`:

```rust
#[derive(Debug, Clone, Default)]
pub(crate) struct WorkflowPanelState {
    selected: usize,
    tasks: Vec<BackgroundTaskSummary>,
}
```

Change `AppState::workflow_panel` to `pub(crate)` and import the state type
from `workflow_panel`. Do not add task revisions, cursors, durable state, or
runtime access to this type.

- [x] **Step 2: Implement owner transitions and immutable queries**

Implement these methods in `workflow_panel.rs`:

```rust
impl WorkflowPanelState {
    pub(crate) fn tasks(&self) -> &[BackgroundTaskSummary];
    pub(crate) fn selected(&self) -> usize;
    pub(crate) fn selected_task(&self) -> Option<&BackgroundTaskSummary>;
    fn replace_tasks(&mut self, tasks: Vec<BackgroundTaskSummary>);
    fn select_previous(&mut self);
    fn select_next(&mut self);
    fn select_index(&mut self, selected: usize);
    pub(crate) fn reset_for_session(&mut self);
}
```

`replace_tasks` must use `sort_workflow_tasks_for_panel`, preserve the prior
selected task id when it remains present, and otherwise clamp the index. The
selection methods must keep an empty list at zero and a nonempty list within
`0..tasks.len()`. Keep direct state mutation confined to this module.

- [x] **Step 3: Route existing AppState behavior through the owner**

Replace the direct state mutations in `show_workflows`, `show_agents`,
`select_previous_workflow_task`, `select_next_workflow_task`,
`open_selected_background_approval_dialog`, and
`apply_workflow_tasks_update` with the new API. Preserve the existing
background-task reveal, approval reveal, foreground return, task sorting, and
notice behavior. `AppState::reset_session_projection_state` calls
`self.reset_workflow_panel()`.

- [x] **Step 4: Run Task 1 GREEN evidence**

Run:

```bash
cargo test -p orca-tui workflow_panel::tests::workflow_panel_owner_sorts_preserves_selection_and_clears --lib --locked -- --exact --test-threads=1
```

Expected: PASS.

### Task 3: Migrate Production Readers And Single-Task Updates

**Files:**
- Modify: `crates/orca-tui/src/types.rs:1345-1351, 2140-2148`
- Modify: `crates/orca-tui/src/ui.rs:903-955, 3160, 4638`
- Modify: `crates/orca-tui/src/workflow_panel_actions.rs:50-74`
- Modify if compiler requires: `crates/orca-tui/src/idle_key_actions.rs`, `crates/orca-tui/src/slash_command_actions.rs`

- [x] **Step 1: Add a focused AppState merge regression test**

After Task 1's RED/GREEN API proof, add a test that applies
`WorkflowTasksUpdated`, selects a task by invoking existing panel navigation,
then applies `WorkflowTaskUpdated` for that task. Assert the updated task is
still selected and its status changed without duplicating the task. The test
must call immutable workflow-panel AppState queries. This protects existing
event behavior while the reader migration removes field access.

- [x] **Step 2: Replace production reads**

Add AppState query methods in `workflow_panel.rs`:

```rust
pub(crate) fn workflow_tasks(&self) -> &[BackgroundTaskSummary];
pub(crate) fn workflow_selected_index(&self) -> usize;
pub(crate) fn selected_workflow_task(&self) -> Option<&BackgroundTaskSummary>;
```

Use them in rendering and keyboard action selection. In the `WorkflowTaskUpdated`
reducer arm, clone `self.workflow_tasks()`, replace or append by id, and pass
the full list to `apply_workflow_tasks_update`. Preserve every existing event
variant and runtime action.

- [x] **Step 3: Run Task 3 regression evidence**

```bash
cargo test -p orca-tui types::tests::workflow_task_update_preserves_owner_selection --lib --locked -- --exact --test-threads=1
cargo test -p orca-tui workflow_panel --lib --locked -- --test-threads=1
cargo test -p orca-tui background_approval --lib --locked -- --test-threads=1
```

Expected: all pass with existing selection, approval, backgrounding, and
foregrounding behavior.

### Task 4: Migrate Test Fixtures Without Restoring Mutable Production Fields

**Files:**
- Modify: `crates/orca-tui/src/types.rs` test module
- Modify: `crates/orca-tui/src/app.rs` test module
- Modify: `crates/orca-tui/src/ui.rs` test module
- Modify: `crates/orca-tui/src/input_event_actions.rs` test module

- [x] **Step 1: Provide minimal test-only setup methods**

In `workflow_panel.rs`, add narrowly scoped `#[cfg(test)]` AppState helpers:

```rust
#[cfg(test)]
pub(crate) fn replace_workflow_tasks_for_test(&mut self, tasks: Vec<BackgroundTaskSummary>);

#[cfg(test)]
pub(crate) fn select_workflow_index_for_test(&mut self, selected: usize);
```

The replacement helper must call the same sorted owner transition as production
refreshes. Adapt fixtures to their user-visible sorted order rather than adding
a test-only bypass. The selection helper clamps its input. Do not make state
fields public again.

- [x] **Step 2: Replace direct test assignments and reads**

Migrate each test occurrence of `workflow_panel.tasks` and
`workflow_panel.selected` to the helpers or immutable AppState queries. For a
test that changes a task status, construct the desired replacement task list
and install it through the helper instead of mutating an element in place.

- [x] **Step 3: Run focused tests**

```bash
cargo test -p orca-tui workflow --lib --locked -- --test-threads=1
cargo test -p orca-tui backgrounded --lib --locked -- --test-threads=1
cargo test -p orca-tui surface_projection_consistency --lib --locked -- --test-threads=1
```

Expected: all pass. Any changed fixture must still assert a user-observable
panel, approval, task, or projection outcome.

### Task 5: Verify the Boundary and Refresh Documentation

**Files:**
- Modify: `docs/production-roadmap.md`
- Modify: `docs/superpowers/specs/2026-08-15-tui-workflow-panel-state-owner.md`
- Modify: `docs/superpowers/plans/2026-08-15-tui-workflow-panel-state-owner.md`
- Modify only if validation proves factual drift: `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json` and its digest

- [x] **Step 1: Search for forbidden production bypasses**

Run:

```bash
rg -n -U 'workflow_panel\s*\.\s*(tasks|selected)' crates/orca-tui/src --glob '*.rs' -g '!workflow_panel.rs'
rg -n -U 'struct WorkflowPanelState \{\n\s+pub' crates/orca-tui/src/workflow_panel.rs
rg -n 'TaskPatch::Reconciled' crates/orca-runtime/src --glob '*.rs'
```

Expected: no cross-module direct field access; the state has private fields;
the final search remains declaration/serializer/reducer support only, proving
this TUI slice did not fabricate a runtime reconciliation producer.

- [x] **Step 2: Update the roadmap after behavior is green**

Add a concise completed-slice record that private workflow-panel presentation
state owns selection and task-list transitions. Preserve the explicit note that
runtime task reconciliation remains deferred because non-recorded
`TaskRegistry` summaries have no production `TaskPatch::Reconciled` source.

- [x] **Step 3: Run the runtime-surface validator**

```bash
node scripts/validate-runtime-surface-contract.mjs
```

Update manifest anchors or digest only if the validator reports factual drift;
do not weaken the contract or add a baseline.

### Task 6: Full Verification, Review, Commit, Rebase, And Integration

**Files:**
- Modify: only the reviewed files from Tasks 1-5

- [x] **Step 1: Run full TUI and contract gates**

```bash
cargo check -p orca-tui --tests --locked
cargo test -p orca-tui --lib --locked -- --test-threads=1
cargo test --test tui_pty_contract --locked -- --test-threads=1
node --test scripts/test-validate-runtime-surface-contract.mjs
node --test scripts/test-validate-windows-platform-boundaries.mjs
node scripts/validate-runtime-surface-contract.mjs
cargo fmt --all -- --check
git diff --check
```

Expected: every command exits zero. Treat existing warnings as baseline only;
do not broaden this state-owner slice with unrelated cleanup.

- [x] **Step 2: Independently review the complete diff**

Review state ownership, index clamping, selected-id preservation, reset,
background and approval reveal, foreground return, single-task merge,
production-field bypasses, runtime-task source preservation, public API impact,
and test coverage. Resolve every Critical or Important finding, rerun affected
tests, and repeat review until no such finding remains.

- [x] **Step 3: Commit the semantic slice**

```bash
git add crates/orca-tui/src/types.rs crates/orca-tui/src/workflow_panel.rs crates/orca-tui/src/workflow_panel_actions.rs crates/orca-tui/src/slash_command_actions.rs crates/orca-tui/src/ui.rs crates/orca-tui/src/app.rs crates/orca-tui/src/input_event_actions.rs docs/production-roadmap.md docs/superpowers/specs/2026-08-15-tui-workflow-panel-state-owner.md docs/superpowers/plans/2026-08-15-tui-workflow-panel-state-owner.md
git commit -m "refactor(tui): own workflow panel state"
```

Stage additional TUI files only when compiler evidence proves they are callers
of the private owner API; do not stage unrelated edits.

- [x] **Step 4: Rebase and reverify before local main integration**

```bash
git fetch origin main --prune
git rebase main
cargo test -p orca-tui workflow --lib --locked -- --test-threads=1
cargo test -p orca-tui --lib --locked -- --test-threads=1
cargo test --test tui_pty_contract --locked -- --test-threads=1
node scripts/validate-runtime-surface-contract.mjs
cargo fmt --all -- --check
git diff --check
```

With a clean local main checkout, fast-forward only this reviewed commit into
local `main`, rerun the affected gates there, then remove only this slice's
worktree and merged branch. Do not push, tag, or publish.
