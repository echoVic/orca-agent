# TUI Side Background Reentry Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Keep a backgrounded Side conversation's task projection live after reentry without weakening attachment fencing or foregrounding the operation.

**Architecture:** The runtime remains the task owner. Parent-to-Side activation rotates the attachment, publishes reset/history, then asks `surface_client` to replace live background monitors with monitors bound to the new sender. The existing `TuiSurfaceTaskControl` serializes replacement for each operation id.

**Tech Stack:** Rust, crossbeam channels, typed runtime surface snapshots, Ratatui TUI events, Cargo tests.

---

### Task 1: Prove Reentry Loses The Old Monitor

**Files:**
- Modify: `crates/orca-tui/src/app.rs`

- [x] **Step 1: Add an attached-event regression harness and RED test**

Add a test-only hosted-controller harness that preserves `TuiEvent::Attached` envelopes, routes each through `accept_attached_tui_event`, and applies accepted payloads to `AppState`. Do not use `spawn_unwrapped_tui_test_event_sender`: attachment fencing is the behavior under test.

```rust
#[test]
fn hosted_side_reentry_rebinds_background_presentation_to_active_attachment() {
    // Start a delayed Side turn and capture its running, backgrounded
    // MainSession task id. Toggle Side -> parent -> Side through the real
    // attachment filter. Require the captured task's terminal update and
    // assert it remains backgrounded.
}
```

- [x] **Step 2: Run RED evidence**

```bash
cargo test -p orca-tui --lib hosted_side_reentry_rebinds_background_presentation_to_active_attachment --locked -- --test-threads=1
```

Expected: FAIL by timing out for the terminal update, because the old monitor remains attached to the retired Side generation.

### Task 2: Rebind Background Presentation

**Files:**
- Modify: `crates/orca-tui/src/surface_client.rs`
- Modify: `crates/orca-tui/src/app.rs`

- [x] **Step 1: Add a `surface_client` rebind helper**

```rust
pub(crate) fn rebind_background_presentations(
    thread: &RuntimeSurfaceThreadHandle,
    controller: &TuiSurfaceTaskControl,
    event_tx: mpsc::Sender<TuiEvent>,
) -> Result<(), String> {
    let snapshot = read_snapshot(thread).map_err(|error| error.to_string())?;
    let surface = thread.surface();
    for background in snapshot.background_operations {
        spawn_background_presentation(
            &surface,
            background.operation_id,
            controller,
            event_tx.clone(),
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}
```

This issues no task-control mutation. The snapshot lists only live background operations, and the controller joins a replaced monitor before the new one can publish.

- [x] **Step 2: Invoke it after Side reset/history succeeds**

In the parent-to-Side branch of `UserAction::ToggleSideConversation`, call the helper after `project_hosted_thread_attached` and before `SideConversationChanged` or `announce_runtime_ready`. Report but do not roll back an error:

```rust
let _ = event_tx.send(TuiEvent::OperationRejected(format!(
    "failed to reattach side background task presentation: {error}"
)));
```

- [x] **Step 3: Run GREEN evidence**

```bash
cargo test -p orca-tui --lib hosted_side_reentry_rebinds_background_presentation_to_active_attachment --locked -- --test-threads=1
```

Expected: PASS with the terminal update from the reactivated Side attachment.

### Task 3: Verify Lifecycle Boundaries

**Files:**
- Modify: `crates/orca-tui/src/app.rs` test module only if the RED path needs a bounded helper.

- [x] **Step 1: Keep background ownership in the assertion**

The behavior test must assert the terminal `BackgroundTaskSummary` uses the captured task id and has `is_backgrounded == true`. It must not use `UserAction::ForegroundTask`.

- [x] **Step 2: Run focused tests**

```bash
cargo test -p orca-tui side_reentry --lib --locked -- --test-threads=1
cargo test -p orca-tui backgrounded --lib --locked -- --test-threads=1
cargo test -p orca-tui attachment_routing --lib --locked -- --test-threads=1
cargo test -p orca-tui --lib app::tests::hosted_side_switches_project_recorded_parent_and_ephemeral_side_identity --locked -- --exact --test-threads=1
```

Expected: reentry, background handoff, stale generation fencing, and Side identity pass.

### Task 4: Document, Verify, Review, And Integrate

**Files:**
- Modify: `docs/production-roadmap.md`
- Modify: `docs/superpowers/specs/2026-08-15-tui-side-background-reentry.md`
- Modify: `docs/superpowers/plans/2026-08-15-tui-side-background-reentry.md`

- [x] **Step 1: Record the boundary**

Add a roadmap entry that Side attachment rotation fences stale output while monitor replacement restores task projection only. Runtime ownership and explicit foreground control remain unchanged.

- [x] **Step 2: Run full gates**

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

Expected: every command exits zero. Do not change the runtime-surface manifest or digest unless the validator reports factual drift.

- [x] **Step 3: Review, commit, rebase, integrate, and clean up**

Review background ownership, attachment ordering, replacement join behavior, error reporting, task identity, and stale-event fencing. Resolve every Critical or Important finding, then run:

```bash
git add crates/orca-tui/src/app.rs crates/orca-tui/src/surface_client.rs docs/production-roadmap.md docs/superpowers/specs/2026-08-15-tui-side-background-reentry.md docs/superpowers/plans/2026-08-15-tui-side-background-reentry.md
git commit -m "fix(tui): rebind side background presentation"
git fetch origin main --prune
git rebase main
cargo test -p orca-tui side_reentry --lib --locked -- --test-threads=1
cargo test -p orca-tui --lib --locked -- --test-threads=1
cargo test --test tui_pty_contract --locked -- --test-threads=1
node scripts/validate-runtime-surface-contract.mjs
cargo fmt --all -- --check
git diff --check
```

With a clean local `main`, fast-forward only the reviewed commit, rerun the focused reentry test and PTY contract there, then remove only this slice's worktree and merged branch. Do not push, tag, or publish.

## Plan Self-Review

The plan maps the stale-sender root cause to one attachment-preserving behavior test, one monitor-rebind capability, and one activation call. It preserves runtime background ownership, uses the existing cancel-and-join monitor owner, covers terminal task visibility and stale-event fencing, and changes no protocol, persistence, or second fact source.
