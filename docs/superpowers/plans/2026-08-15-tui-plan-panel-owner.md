# TUI Plan Panel State Ownership Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the TUI plan panel one explicit state owner while preserving its existing event, history, archive, and rendering behavior.

**Architecture:** `plan_panel.rs` owns private live-plan and stale-marker facts plus explicit transitions. `types.rs` remains the event reducer and transcript owner, delegating plan facts to AppState commands. `ui.rs` receives immutable plan and stale queries only.

**Tech Stack:** Rust 2024, Cargo, ratatui, `orca_core::plan_types`, existing TUI event reducer.

---

### Task 1: Add The Failing Aggregate Contract Test

**Files:**
- Create: `crates/orca-tui/src/plan_panel.rs`
- Modify: `crates/orca-tui/src/lib.rs`

- [x] **Step 1: Add the module declaration and a RED state-contract test**

```rust
#[test]
fn plan_panel_replaces_marks_stale_and_transfers_archive_once() {
    let mut panel = PlanPanelState::default();
    panel.apply_update(Some("inspect".to_string()), vec![item("Inspect")]);
    panel.mark_update_failed();
    assert!(panel.update_failed());

    panel.apply_update(None, Vec::new());
    assert!(panel.current_plan().is_none());
    assert!(!panel.update_failed());

    panel.apply_update(None, vec![item("Patch")]);
    assert_eq!(panel.take_for_archive().unwrap().1[0].step, "Patch");
    assert!(panel.current_plan().is_none());
    assert!(panel.take_for_archive().is_none());
}
```

- [x] **Step 2: Run the exact test and verify RED**

Run: `cargo test -p orca-tui plan_panel_replaces_marks_stale_and_transfers_archive_once --lib --locked -- --test-threads=1`

Expected: FAIL because `plan_panel` and `PlanPanelState` do not exist.

### Task 2: Implement The Aggregate And AppState Commands

**Files:**
- Modify: `crates/orca-tui/src/plan_panel.rs`
- Modify: `crates/orca-tui/src/lib.rs`
- Modify: `crates/orca-tui/src/types.rs`

- [x] **Step 1: Implement the private aggregate and immutable observations**

```rust
pub(crate) struct PlanPanelState {
    current_plan: Option<(Option<String>, Vec<PlanItem>)>,
    update_failed: bool,
}

impl PlanPanelState {
    pub(crate) fn apply_update(&mut self, explanation: Option<String>, plan: Vec<PlanItem>) {
        self.update_failed = false;
        self.current_plan = (!plan.is_empty()).then_some((explanation, plan));
    }

    pub(crate) fn take_for_archive(&mut self) -> Option<(Option<String>, Vec<PlanItem>)> {
        self.update_failed = false;
        self.current_plan.take().filter(|(_, plan)| !plan.is_empty())
    }
}
```

Implement `restore`, `reset_for_session`, `mark_update_failed`, `current_plan`, and
`update_failed` with the exact contract from the spec. Add AppState commands
and public immutable `current_plan()` / `plan_update_failed()` getters in this
module; add only cfg(test) setup needed by renderer tests.

- [x] **Step 2: Replace direct fields and reducer writes**

```rust
// AppState fields
pub(crate) plan_panel: PlanPanelState,

// PlanUpdated reducer branch
self.apply_plan_update(explanation, plan);

// failed update_plan tool result
self.mark_plan_update_failed();

// turn completion
if let Some((explanation, plan)) = self.take_plan_for_archive() {
    self.push_message(ChatMessage::PlanUpdate { explanation, plan });
}
```

Initialize with `PlanPanelState::default()`. Route history restoration and
session reset through commands. Delete `current_plan` and `plan_update_failed`
fields and every production direct assignment.


- [x] **Step 3: Run focused aggregate and reducer tests**

Run:

```bash
cargo test -p orca-tui plan_panel_replaces_marks_stale_and_transfers_archive_once --lib --locked -- --test-threads=1
cargo test -p orca-tui plan_lives_in_panel_during_turn_and_archives_inline_on_completion --lib --locked -- --test-threads=1
cargo test -p orca-tui failed_plan_update_marks_panel_stale --lib --locked -- --test-threads=1
cargo test -p orca-tui turn_completion_clears_plan_stale_marker --lib --locked -- --test-threads=1
```

Expected: PASS, including the new aggregate test and existing behavioral tests.

### Task 3: Migrate Renderer And Test Setups

**Files:**
- Modify: `crates/orca-tui/src/ui.rs`
- Modify: affected tests in `crates/orca-tui/src/types.rs`

- [x] **Step 1: Replace renderer field reads with immutable queries**

```rust
let Some((_, plan)) = state.current_plan() else {
    return;
};
let stale = state.plan_update_failed();
```

Use this in both plan-panel height and rendering. Preserve every height, title,
and color rule.

- [x] **Step 2: Replace direct test setup**

```rust
state.replace_plan_for_test(Some((
    None,
    vec![PlanItem {
        step: "fixture".to_string(),
        status: PlanStatus::Pending,
    }],
)));
```

Keep behavior tests event-driven where possible. The renderer fixture may use
the cfg(test) setter, but it must never obtain mutable aggregate state.

- [x] **Step 3: Run renderer and full focused tests**

Run:

```bash
cargo test -p orca-tui plan_ --lib --locked -- --test-threads=1
cargo test -p orca-tui long_plan_steps_and_tool_targets_stay_on_single_rows --lib --locked -- --test-threads=1
```

Expected: PASS.

### Task 4: Record Boundaries And Verify The Slice

**Files:**
- Modify: `docs/production-roadmap.md`
- Modify: `docs/superpowers/specs/2026-08-15-tui-plan-panel-owner.md`
- Modify: `docs/superpowers/plans/2026-08-15-tui-plan-panel-owner.md`

- [x] **Step 1: Update the roadmap after behavioral tests are green**

Record that `PlanPanelState` owns only process-local TUI presentation facts;
existing `PlanUpdated` and history restoration remain inputs because legacy
structured-plan hydration has not moved into the runtime surface.

- [x] **Step 2: Run the ownership scan and direct validator**

Run:

```bash
rg -n 'current_plan:|plan_update_failed:' crates/orca-tui/src -g '*.rs'
rg -n 'current_plan\\s*=|plan_update_failed\\s*=' crates/orca-tui/src -g '*.rs'
node scripts/validate-runtime-surface-contract.mjs
```

Expected: the first two searches find only private aggregate state or test-only
fixtures; the validator passes without a baseline change.

- [x] **Step 3: Run complete gates**

Run:

```bash
cargo test -p orca-tui --lib --locked -- --test-threads=1
cargo test --test tui_pty_contract --locked -- --test-threads=1
node --test scripts/test-validate-runtime-surface-contract.mjs
node --test scripts/test-validate-windows-platform-boundaries.mjs
cargo fmt --all -- --check
git diff --check
```

Expected: all commands pass.

### Task 5: Review, Rebase, Commit, And Integrate

**Files:**
- Create: `crates/orca-tui/src/plan_panel.rs`
- Modify: `crates/orca-tui/src/lib.rs`
- Modify: `crates/orca-tui/src/types.rs`
- Modify: `crates/orca-tui/src/ui.rs`
- Modify: `docs/production-roadmap.md`
- Modify: `docs/superpowers/specs/2026-08-15-tui-plan-panel-owner.md`
- Modify: `docs/superpowers/plans/2026-08-15-tui-plan-panel-owner.md`

- [x] **Step 1: Review the diff**

Confirm one private aggregate, no direct production mutation outside it, no
event/protocol/persistence change, unchanged archive behavior, immutable UI
access, and an explicit legacy-history boundary.

- [x] **Step 2: Commit the verified slice**

Run:

```bash
git add crates/orca-tui/src/plan_panel.rs crates/orca-tui/src/lib.rs crates/orca-tui/src/types.rs crates/orca-tui/src/ui.rs docs/production-roadmap.md docs/superpowers/specs/2026-08-15-tui-plan-panel-owner.md docs/superpowers/plans/2026-08-15-tui-plan-panel-owner.md
git commit -m "refactor(tui): own plan panel state"
```

- [ ] **Step 3: Rebase and repeat the gates**

Fetch only if necessary, rebase on the current local `main`, and rerun Task 4
plus the focused plan tests. Fast-forward local `main` only after the rebased
commit and verification pass. Remove only this slice worktree and branch; do
not push, tag, or publish.

## Plan Self-Review

The aggregate, reducer, history restore, reset, archive, renderer, test setup,
read access, documented legacy boundary, validation, commit, and
integration all map to a concrete task. The plan introduces neither a second
plan fact source nor a runtime migration masquerading as a UI refactor.
