# TUI Input History Ownership Extraction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox syntax for tracking.

**Goal:** Give TUI input-history behavior one module owner without changing recall, draft restoration, durable history, or input routing.

**Architecture:** `input_history.rs` owns local history I/O and the existing AppState recording and navigation methods. `AppState` remains the only holder of the history vector, navigation cursor, and draft snapshot; input routing keeps calling the same AppState API.

**Tech Stack:** Rust 2024, Cargo, Node contract validators, reviewed runtime-surface manifest/digest, existing PTY contract harness.

---

### Task 1: Characterize Existing Recall Behavior

**Files:** Create `crates/orca-tui/src/input_history.rs`; modify `crates/orca-tui/src/lib.rs`.

- [x] Add a test named `history_navigation_clamps_and_restores_the_unsent_draft`. Seed the existing AppState history with `first` and `second`; assert Up returns `second`, then `first`, then `None`; assert Down returns `second`, then the saved draft, then `None`; assert restoration and an explicit `reset_history_navigation` call both clear the cursor and draft.
- [x] Register `mod input_history;` in `lib.rs`.
- [x] Run `cargo test -p orca-tui history_navigation_clamps_and_restores_the_unsent_draft --lib --locked -- --test-threads=1`. This characterization test passes before the move because the slice intentionally preserves existing behavior.

### Task 2: Move The Input History Policy

**Files:** Modify `crates/orca-tui/src/input_history.rs` and `crates/orca-tui/src/types.rs`.

- [x] Move `input_history_path`, `current_project`, `load_input_history`, and `append_input_history` verbatim to `input_history.rs`; mark only `load_input_history` `pub(crate)`.
- [x] Import `crate::input_history::load_input_history` into `types.rs` and retain constructor initialization through that function.
- [x] Move the unchanged public `AppState::{record_prompt, history_previous, history_next, reset_history_navigation}` implementation to `input_history.rs`; delete the four old definitions from `types.rs`. Preserve duplicate suppression, project ordering, current 500-entry bound, draft capture, oldest clamping, forward traversal, restoration, and reset semantics.
- [x] Run the focused test again. Expected result: PASS from the relocated implementation.

### Task 3: Validate The Contract Boundary

**Files:** Modify `scripts/validate-runtime-surface-contract.mjs`, the reviewed runtime-surface manifest and digest, and `docs/production-roadmap.md`.

- [x] Replace only the two harmless-inventory keys for `append_input_history` and `load_input_history` from `types.rs` to `input_history.rs`; do not change `record_prompt` caller inventories.
- [x] Move the three reviewed `input_history` entrypoint anchors in `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.manifest.json` to the new module definitions, then refresh that manifest's SHA-256 in `docs/superpowers/specs/2026-07-21-runtime-owned-typed-surface-private-contract.digest.json`.
- [x] Add the roadmap statement that input-history I/O and draft-restoring navigation have one module owner, AppState remains the sole state aggregate, and no external protocol changes.
- [x] Run `cargo test -p orca-tui --lib --locked -- --test-threads=1`.
- [x] Run `cargo test --test tui_pty_contract --locked -- --test-threads=1`.
- [x] Run `node --test scripts/test-validate-runtime-surface-contract.mjs` and `node --test scripts/test-validate-windows-platform-boundaries.mjs`.
- [x] Run `cargo fmt --all -- --check` and `git diff --check`. Every command must exit 0.

### Task 4: Synchronize And Commit

**Files:** All files modified above.

- [x] Fetch and rebase on `origin/main`; rerun both Cargo suites and both Node validators.
- [x] Review `git diff --word-diff=plain origin/main -- crates/orca-tui/src/input_history.rs crates/orca-tui/src/types.rs`; verify no duplicate policy, protocol change, persistence change, or inventory drift beyond the two relocated definitions.
- [x] Commit all slice files with `refactor(tui): own input history policy in its module`.

## Plan Self-Review

Task 1 provides the user-visible behavior oracle; Task 2 removes the old policy and retains one state source; Task 3 covers every acceptance gate, the validator inventory, and reviewed manifest/digest artifacts; Task 4 fulfills rebase, review, and one semantic commit. Method names and signatures stay identical, and no temporary compatibility path exists.
