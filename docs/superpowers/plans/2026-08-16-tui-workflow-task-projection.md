# Workflow Task Projection Ownership Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the accepted runtime surface snapshot the only production TUI source for workflow-panel task facts, including ephemeral Side foreground control.

**Architecture:** Keep native runtime `TaskPatch`, `WorkflowPatch`, and `SubagentPatch` commits unchanged. Collapse all TUI task replacement onto the final `SurfaceProjectionSynced` envelope, make task-control facades return a post-commit projection, and delete granular full-list/single-task events plus registry fallbacks. `WorkflowPanelState` remains the presentation owner and applies each admitted snapshot through its existing full-list transition.

**Tech Stack:** Rust, crossbeam channels, Tokio-owned runtime surface actor, Cargo tests, Node runtime-surface validators.

---

## File Map

- `crates/orca-tui/src/surface_projection.rs`: stop projecting a separate task
  list; retain the final snapshot and add the RED unit characterization.
- `crates/orca-tui/src/surface_client.rs`: return post-commit projection states,
  remove ephemeral/registry bypasses, and publish snapshot envelopes from
  background monitors.
- `crates/orca-tui/src/surface_actions.rs`: expose projection return types from
  the crate-private TUI facade.
- `crates/orca-tui/src/background_tasks.rs`: publish post-commit projections for
  stop, foreground, and recovered approval visibility.
- `crates/orca-tui/src/background_approval.rs`: publish a post-commit projection
  before the existing decision notice.
- `crates/orca-tui/src/types.rs`: delete both granular task event variants and
  reducers; retain the workflow-panel transition under accepted projections.
- `crates/orca-tui/src/workflow_panel.rs`: expose the test-only full-list
  transition helper used after deleting granular event fixtures.
- `scripts/validate-runtime-surface-contract.mjs`: refresh the reviewed hash for
  the intentionally changed `surface_client::stop_task` associated-function
  anchor.
- `crates/orca-tui/src/app.rs`: remove startup duplication, migrate integration
  helpers/assertions to snapshot events, and add the ephemeral foreground RED
  test.
- `crates/orca-tui/src/ui.rs`: migrate the remaining event-based test fixture to
  the existing test-only workflow-panel helper.
- `docs/production-roadmap.md`: record the completed projection boundary and
  correct stale operation/workflow wording and line counts.
- `docs/superpowers/specs/2026-08-16-tui-workflow-task-projection.md`: keep status
  and acceptance evidence aligned with the implementation.

### Task 1: Prove One Task Snapshot Per Typed Batch

**Files:**
- Modify: `crates/orca-tui/src/surface_projection.rs`

- [x] **Step 1: Add a task batch fixture and failing projection test**

In `surface_projection::tests`, construct a `SurfaceTask` with revision one and
a one-event `TaskPatch::Upserted` batch from `goal_projection_snapshot()`. The
test must require the whole projected vector to be one final snapshot:

```rust
#[test]
fn task_patch_projects_one_authoritative_snapshot() {
    let snapshot = goal_projection_snapshot();
    let mut projection = TuiSurfaceProjection::from_surface_snapshot(&snapshot);
    let task = SurfaceTask {
        task_id: SurfaceTaskId::try_new("task-projection-1").unwrap(),
        revision: TaskRevision::try_new(1).unwrap(),
        task_type: SurfaceTaskType::Workflow,
        status: SurfaceTaskStatus::Running,
        backgrounded: false,
        description: DisplayText::new("project one task snapshot"),
        created_at: UnixMillis::new(1_000),
        started_at: Some(UnixMillis::new(1_000)),
        completed_at: None,
        parent_operation: None,
        background_fence: None,
        workflow_run_id: None,
        subagent_id: None,
        pending_interaction_id: None,
        usage: None,
        result: None,
        error: None,
        retry_count: 0,
        output_truncated: false,
    };
    let batch = task_projection_batch(&projection, 31, task.clone());
    let events = projection.project_typed_batch(&batch).unwrap();
    let [TuiEvent::SurfaceProjectionSynced(state)] = events.as_slice() else {
        panic!("task batch must end in one projection: {events:?}");
    };
    assert_eq!(state.cursor, batch.cursor_after);
    assert!(state.workflow_tasks.iter().any(|item| item.id == task.task_id.as_str()));
}
```

`task_projection_batch` must follow `goal_projection_batch`: one recorded commit,
thread scope, `SurfaceEvent::Task(TaskPatch::Upserted { expected_revision: None,
task })`, one sequence increment, and a canonical batch digest.

- [x] **Step 2: Run the RED test**

```bash
cargo test -p orca-tui surface_projection::tests::task_patch_projects_one_authoritative_snapshot --lib --locked -- --exact --test-threads=1
```

Expected: FAIL because the current result contains
`WorkflowTasksUpdated` before `SurfaceProjectionSynced`.

- [x] **Step 3: Remove the duplicate typed-batch task-list projection**

Delete the block in `reduce_typed_batch` that appends
`TuiEvent::WorkflowTasksUpdated` for task/workflow events. Do not remove task,
workflow, or subagent families from `needs_projection_snapshot`; those families
must still produce the final snapshot.

- [x] **Step 4: Run the RED test again**

Run the exact command from Step 2.

Expected: PASS with one `SurfaceProjectionSynced` carrying the task.

### Task 2: Make Task Actions Return Post-Commit Projections

**Files:**
- Modify: `crates/orca-tui/src/surface_client.rs`
- Modify: `crates/orca-tui/src/surface_actions.rs`
- Modify: `crates/orca-tui/src/background_tasks.rs`
- Modify: `crates/orca-tui/src/background_approval.rs`

- [x] **Step 1: Change the surface-client return boundary**

Change these signatures:

```rust
pub(crate) fn stop_task(...) -> Result<SurfaceProjectionState, String>
pub(crate) fn foreground_task(...) -> Result<SurfaceProjectionState, String>
pub(crate) fn resolve_background_approval(...)
    -> Result<(String, SurfaceProjectionState), String>
```

At every successful terminal branch, read the authoritative snapshot and return
`SurfaceProjectionState::from_surface_snapshot(&snapshot)`. For the foreground
terminal-baseline branch, project `attachment.baseline.snapshot`. Remove:

```rust
if thread.session_id().is_none() {
    return thread.foreground_task(task_id);
}
```

and remove the workflow-stop merge with `thread.task_summaries()`. A missing
surface task must remain a typed `surface task ... not found` error.

- [x] **Step 2: Propagate projection types through `TuiSurfaceActions`**

Change `recoverable_background_approval_projection` to return
`Result<(SurfaceProjectionState, Vec<String>), String>`, deriving the projection
from the same snapshot used to locate requested interactions. Change stop,
foreground, and approval methods to return the new surface-client types. Remove
the now-unused `BackgroundTaskSummary` import.

- [x] **Step 3: Publish projection then notice from action helpers**

For each successful task action, use:

```rust
let _ = event_tx.send(TuiEvent::SurfaceProjectionSynced(Box::new(projection)));
let _ = event_tx.send(TuiEvent::Notice(message));
```

`stop_task_for_tui` must not perform a second snapshot read. Recovered approval
notification must publish the projection only when requested interactions exist.
Approval resolution must preserve `(task_id, projection)` and publish projection
before the approved/denied notice. Error branches must publish neither snapshot
nor success notice.

- [x] **Step 4: Compile the migrated facade**

```bash
cargo check -p orca-tui --tests --locked
```

Expected: compile errors only at remaining granular-event tests/senders targeted
by Tasks 3-4, not in action facade signatures.

### Task 3: Publish Background And Startup State Only As Snapshots

**Files:**
- Modify: `crates/orca-tui/src/surface_client.rs`
- Modify: `crates/orca-tui/src/app.rs`

- [x] **Step 1: Replace background-monitor list publication**

At a fresh background attachment, create
`SurfaceProjectionState::from_surface_snapshot(&attachment.baseline.snapshot)`
and send it through `send_background_presentation_event` as
`SurfaceProjectionSynced`. Derive `snapshot_task` from the same projection and
then call `publish_background_approval_notice`.

For each typed batch, locate the final `SurfaceProjectionSynced` from
`project_typed_batch`, send it once through the attachment-bound presentation
sender, and then issue an approval notice only when the selected task changed.
Delete `publish_background_task_projection`; keep the existing cancellation and
controller-generation checks around every send.

- [x] **Step 2: Remove startup's following task list**

In `emit_typed_history_snapshot`, keep `HistoryLoaded` followed by exactly one
`SurfaceProjectionSynced`. Delete the `workflow_task_summaries` call and
conditional `WorkflowTasksUpdated` send.

- [x] **Step 3: Migrate integration task matching to snapshots**

Change the app test helper to:

```rust
fn matching_task_update(
    event: TuiEvent,
    predicate: impl Fn(&BackgroundTaskSummary) -> bool,
) -> Option<BackgroundTaskSummary> {
    match event {
        TuiEvent::SurfaceProjectionSynced(projection) => {
            projection.workflow_tasks.into_iter().find(predicate)
        }
        _ => None,
    }
}
```

Remove test alternatives that accept granular task events. Where a test inspects
startup events, assert the task inside `SurfaceProjectionSynced` and assert only
one projection event for the hydration batch.

- [x] **Step 4: Verify the focused existing behavior**

```bash
cargo test -p orca-tui app::tests::resumed_registry_only_approval_is_not_advertised_as_actionable --lib --locked -- --exact --test-threads=1
cargo test -p orca-tui app::tests::hosted_side_reentry_rebinds_background_presentation_to_active_attachment --lib --locked -- --exact --test-threads=1
```

Expected: both pass using snapshot-only task matching.

### Task 4: Prove Ephemeral Foreground Uses Typed Surface Control

**Files:**
- Modify: `crates/orca-tui/src/app.rs`

- [x] **Step 1: Add the Side foreground integration regression**

Add `hosted_side_background_task_foreground_uses_surface_projection` beside the
Side reentry test. Use `AttachedHostedTuiHarness` to start a recorded parent,
start Side with `mock_stream_delay_ms 3000`, wait for the slow-stream delta,
background it, and capture the backgrounded main-session task from
`matching_task_update`. Then send:

```rust
harness.send(UserAction::ForegroundTask {
    task_id: task.id.clone(),
});
```

Observe `BackgroundTaskOutputAttached` for that id, accepted snapshots containing
the same id with `is_backgrounded == false`, the terminal assistant/session
presentation, and a terminal-status snapshot before the success notice. The
operation drain is synchronous, so the regression retains those observations
while waiting for the notice and final completion. Do not read
`RuntimeSurfaceThreadHandle::task_summaries` in the test.

- [x] **Step 2: Run the exact integration regression**

```bash
cargo test -p orca-tui app::tests::hosted_side_background_task_foreground_uses_surface_projection --lib --locked -- --exact --test-threads=1
```

Expected: PASS only after the ephemeral registry bypass has been removed and the
typed surface path delivers the projection and output handoff.

- [x] **Step 3: Run focused background and Side suites**

```bash
cargo test -p orca-tui backgrounded --lib --locked -- --test-threads=1
cargo test -p orca-tui side_ --lib --locked -- --test-threads=1
```

Expected: all selected tests pass without a registry-derived task list.

### Task 5: Delete Granular Task Events And Migrate Fixtures

**Files:**
- Modify: `crates/orca-tui/src/types.rs`
- Modify: `crates/orca-tui/src/app.rs`
- Modify: `crates/orca-tui/src/ui.rs`
- Modify: `crates/orca-tui/src/surface_client.rs`

- [x] **Step 1: Delete both TUI event variants and reducer branches**

Remove:

```rust
WorkflowTasksUpdated { tasks: Vec<BackgroundTaskSummary> },
WorkflowTaskUpdated { task: BackgroundTaskSummary },
```

and their `AppState::update` match arms. Keep
`apply_workflow_tasks_update(projection.workflow_tasks.clone())` inside the
accepted projection path.

- [x] **Step 2: Migrate state/unit fixtures without creating a compatibility event**

Tests concerned only with sorting, selection, reveal, approval, or rendering
must call the existing `replace_workflow_tasks_for_test` helper. Tests concerned
with projection admission or attachment fencing must put tasks in a
`SurfaceProjectionState.workflow_tasks` payload. Replace the single-task merge
test with a full-snapshot replacement test proving the same selected-id
invariant; do not add a test-only event variant.

- [x] **Step 3: Remove obsolete alternatives from monitor and integration tests**

Background monitor assertions may accept only `Notice` and
`SurfaceProjectionSynced`. Any loop that previously matched both full and
single-task variants must inspect the final snapshot. Keep terminal notification
and output-handoff assertions unchanged.

- [x] **Step 4: Prove the old rail is absent**

```bash
rg -n 'WorkflowTasksUpdated|WorkflowTaskUpdated' crates/orca-tui/src --glob '*.rs'
rg -n 'thread\.task_summaries\(|RuntimeSurfaceThreadHandle::task_summaries' crates/orca-tui/src --glob '*.rs'
```

Expected: both searches are empty.

- [x] **Step 5: Run projection and panel suites**

```bash
cargo test -p orca-tui surface_projection --lib --locked -- --test-threads=1
cargo test -p orca-tui workflow --lib --locked -- --test-threads=1
cargo test -p orca-tui surface_projection_consistency --lib --locked -- --test-threads=1
```

Expected: all selected tests pass and every task mutation reaches the panel via
an admitted snapshot.

### Task 6: Documentation, Full Gates, Review, Rebase, And Integration

**Files:**
- Modify: `docs/production-roadmap.md`
- Modify: `docs/superpowers/specs/2026-08-16-tui-workflow-task-projection.md`
- Modify: `docs/superpowers/plans/2026-08-16-tui-workflow-task-projection.md`
- Modify only if validator proves anchor drift:
  `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json`
  and its digest

- [x] **Step 1: Update roadmap and implemented status**

Record that accepted surface snapshots now exclusively hydrate task-panel facts,
granular task events and registry fallbacks are deleted, and ephemeral Side
foreground control uses typed surface fences. Correct older statements that
operation projection remains open and distinguish live task projection from
the still-separate cold migration of pre-surface registry rows. Update measured
line counts from fresh `wc -l` output.

- [x] **Step 2: Run focused runtime/TUI and validator gates**

```bash
cargo check -p orca-tui --tests --locked
cargo test -p orca-tui surface_projection::tests::task_patch_projects_one_authoritative_snapshot --lib --locked -- --exact --test-threads=1
cargo test -p orca-tui types::tests::workflow_task_projection_fences_contradictory_equal_cursor --lib --locked -- --exact --test-threads=1
cargo test -p orca-tui app::tests::hosted_side_background_task_foreground_uses_surface_projection --lib --locked -- --exact --test-threads=1
cargo test -p orca-tui app::tests::resumed_registry_only_approval_is_not_advertised_as_actionable --lib --locked -- --exact --test-threads=1
cargo test -p orca-tui workflow --lib --locked -- --test-threads=1
cargo test -p orca-tui backgrounded --lib --locked -- --test-threads=1
cargo test -p orca-tui side_ --lib --locked -- --test-threads=1
cargo test -p orca-runtime runtime_surface --lib --locked -- --test-threads=1
node --test scripts/test-validate-runtime-surface-contract.mjs
node --test scripts/test-validate-windows-platform-boundaries.mjs
node scripts/validate-runtime-surface-contract.mjs
node scripts/validate-windows-platform-boundaries.mjs
```

Expected: every command passes. Manifest files stay unchanged unless the
validator reports a real moved anchor.

- [x] **Step 3: Run full serial TUI and PTY gates**

```bash
cargo test -p orca-tui --lib --locked -- --test-threads=1
cargo test --test tui_pty_contract --locked -- --test-threads=1
cargo fmt --all -- --check
git diff --check
```

Expected: all tests pass; formatter and diff checks are silent.

- [x] **Step 4: Request independent review and address findings**

Review the complete slice against the Spec. Critical and Important findings must
be fixed and all affected focused/full gates rerun. Minor findings must be fixed
when they affect correctness, evidence, or documented workflow.

The first review found that task rows lacked their own contradictory
equal-cursor fence. Add a RED `types::tests::
workflow_task_projection_fences_contradictory_equal_cursor` regression, make a
private task projection owner participate in atomic projection admission and
reset, remove the unused `TuiSurfaceProjection::workflow_task_summaries`
method, correct roadmap evidence, and request re-review before checking this
step.

- [x] **Step 5: Create one semantic commit**

```bash
git add \
  crates/orca-tui/src/app.rs \
  crates/orca-tui/src/background_approval.rs \
  crates/orca-tui/src/background_tasks.rs \
  crates/orca-tui/src/surface_actions.rs \
  crates/orca-tui/src/surface_client.rs \
  crates/orca-tui/src/surface_projection.rs \
  crates/orca-tui/src/types.rs \
  crates/orca-tui/src/ui.rs \
  crates/orca-tui/src/workflow_panel.rs \
  scripts/validate-runtime-surface-contract.mjs \
  docs/production-roadmap.md \
  docs/superpowers/specs/2026-08-16-tui-workflow-task-projection.md \
  docs/superpowers/plans/2026-08-16-tui-workflow-task-projection.md
git commit -m "refactor(tui): own workflow task surface projection"
```

Add manifest/digest paths only if Step 2 proved they changed. Confirm the staged
diff contains no unrelated file.

- [x] **Step 6: Rebase latest local main and reverify**

Fetch `origin/main`, verify the root `main` checkout is clean, then:

```bash
git rebase main
cargo test -p orca-tui surface_projection::tests::task_patch_projects_one_authoritative_snapshot --lib --locked -- --exact --test-threads=1
cargo test -p orca-tui types::tests::workflow_task_projection_fences_contradictory_equal_cursor --lib --locked -- --exact --test-threads=1
cargo test -p orca-tui app::tests::hosted_side_background_task_foreground_uses_surface_projection --lib --locked -- --exact --test-threads=1
cargo test -p orca-tui --lib --locked -- --test-threads=1
cargo test --test tui_pty_contract --locked -- --test-threads=1
node scripts/validate-runtime-surface-contract.mjs
cargo fmt --all -- --check
git diff --check
```

Expected: rebase is clean or conflicts preserve both main behavior and this
single-source boundary; all fresh post-rebase gates pass.

- [x] **Step 7: Integrate on clean local main and clean only this slice**

Fast-forward local `main` to the reviewed branch, rerun the two exact regressions,
full serial TUI suite, PTY contract, runtime-surface validator, formatter, and
diff check from the root checkout. Then remove only
`.worktrees/tui-workflow-task-projection` and delete only
`codex/tui-workflow-task-projection`. Do not push, tag, release, publish, or
touch unrelated worktrees/branches.
