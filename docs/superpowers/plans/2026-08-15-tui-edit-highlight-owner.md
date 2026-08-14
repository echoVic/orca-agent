# TUI Edit Highlight Ownership Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give process-local edit-highlight state and its worker one explicit owner while preserving every stale-result and rendering fallback behavior.

**Architecture:** New `edit_highlight.rs` owns private `EditHighlightState` and the AppState command/query surface. `edit_highlight_worker.rs` retains job mechanics and now owns its join handle through drop. AppState keeps message/revision/cache coordination; UI consumes an immutable derived-map view.

**Tech Stack:** Rust 2024, crossbeam channels, ratatui, syntect-derived highlighting, Cargo, runtime-surface and Windows validators.

### Task 1: Establish Owner And Worker RED Tests

**Files:** Create `crates/orca-tui/src/edit_highlight.rs`; modify `crates/orca-tui/src/lib.rs` and `crates/orca-tui/src/edit_highlight_worker.rs` tests.

- [x] Add `reconfigure_retires_runtime_and_clears_applied_styles_atomically` against absent `EditHighlightState`. Seed a runtime and applied revision, configure a new root/theme/color tuple, then assert runtime absent, applied empty, and the exact tuple installed.
- [x] Run that exact test and confirm RED because the aggregate does not exist.
- [x] Add `drop_closes_job_channel_and_joins_worker`. Use a test worker that marks exit after observing sender disconnect; make current behavior deterministically RED by delaying its exit before the marker.
- [x] Run the worker test and confirm RED because runtime drop returns before the worker marker.

### Task 2: Implement Aggregate And Joined Worker Ownership

**Files:** Modify `crates/orca-tui/src/edit_highlight.rs` and `crates/orca-tui/src/edit_highlight_worker.rs`.

- [x] Implement `EditHighlightState` with private workspace, syntax tuple, optional runtime, applied map, and test hooks; implement the specified defaults and atomic configure transition.
- [x] Move `AppliedDiffHighlight` and test hook aliases beside the aggregate. Add immutable map/config/runtime/pending/test observations only where existing callers require them.
- [x] Store the worker `JoinHandle` and shutdown fence in `EditHighlightRuntime`; raise the fence, close the optional job sender and result receiver, and then join in `Drop`. Check the fence before/during coalescing and before computation so retirement cannot drain an unbounded queued backlog. Preserve ordinary submission, coalescing, result draining, and disconnect behavior.
- [x] Make submit return false after sender loss and ensure no pending cache survives a failed send.
- [x] Run both RED tests and `cargo test -p orca-tui edit_highlight_worker --lib --locked -- --test-threads=1`; expect GREEN.

### Task 3: Move AppState Policy And Replace Fields

**Files:** Modify `crates/orca-tui/src/types.rs`, `crates/orca-tui/src/edit_highlight.rs`, and `crates/orca-tui/src/lib.rs`.

- [x] Replace the seven AppState facts with one `pub(crate) edit_highlights: EditHighlightState`, initialized with `Default`.
- [x] Move configuration, tick/poll, target resolution, job submit, stale-result apply, derived-style lookup, pending cancellation, map removal/pruning, and test query/injection methods from `types.rs` into `edit_highlight.rs`.
- [x] Delegate every fact transition to the aggregate while AppState continues to own message revision advancement, transcript-cache invalidation, and selection invalidation.
- [x] Keep existing AppState command names used by production callers; do not add mutable aggregate access or a second cache.
- [x] Run `cargo check -p orca-tui --tests --locked` and the focused edit-highlight suite.

### Task 4: Migrate Renderer And Behavioral Tests

**Files:** Modify `crates/orca-tui/src/ui.rs`, `crates/orca-tui/src/types.rs`, and affected tests in `app.rs`/`workspace_config.rs` if compile errors identify them.

- [x] Replace renderer map-field access with the aggregate's immutable applied-map query while preserving disjoint cache borrowing and exact revision/tool-id validation.
- [x] Replace test-only direct syntax/map/runtime mutations with explicit test setters/queries; setups must still drive real AppState transitions wherever practical.
- [x] Retain all existing malformed diff, target identity, symlink, revision, reused id, lifecycle pruning, disconnect/respawn, and render-cache assertions.
- [x] Run focused edit-highlight, stale-result, syntax-workspace, renderer, and app polling suites; expect GREEN.
- [x] Search for the seven obsolete AppState fields and direct aggregate fact access outside `edit_highlight.rs`; expect no fact mutation leak.

### Task 5: Synchronize Contracts, Docs, And Full Gates

**Files:** Modify `scripts/validate-runtime-surface-contract.mjs`, `docs/production-roadmap.md`, this spec, and this plan. Modify the reviewed manifest/digest only if the validator reports reviewed entrypoint drift.

- [x] Run the direct validator first; relocate/remove only scanner baselines whose containing functions or collection sites actually moved.
- [x] Update the roadmap with aggregate/worker/AppState/renderer boundaries, joined shutdown, process-local fallback semantics, and unchanged protocols. Mark status/checks only after evidence exists.
- [x] Run full TUI lib and root PTY suites, both Node validator suites, direct contract validation, `cargo fmt --all -- --check`, `git diff --check`, and ownership searches.
- [x] Request independent review focused on worker join/drop order, stale result consumption, message revision ordering, target identity, renderer borrowing, fact leaks, and missing tests; address all Critical and Important findings.

### Task 6: Rebase, Verify, Commit, And Integrate

**Files:** All slice files.

- [ ] Commit once with `refactor(tui): own edit highlight state and worker` after the pre-commit gates.
- [ ] Fetch and rebase on current local `main`/`origin/main`, then rerun the focused owner test, full TUI/PTY suites, validator suites, formatting, diff, and ownership checks.
- [ ] Review the committed diff for a single aggregate, one joined worker, no protocol/persistence change, no second cache, and no unrelated cleanup.
- [ ] Use `superpowers:finishing-a-development-branch`; fast-forward clean local main, reverify the integrated tree, remove only this owned worktree/branch, and do not push or publish this architecture slice.

## Plan Self-Review

Every spec transition maps to Tasks 1-4; worker lifecycle, deletion, contracts, docs, review, rebase, and integration map to Tasks 2, 5, and 6. The slice changes derived presentation ownership only. No placeholder, dual state, public protocol change, source-shape behavior oracle, or unbounded shutdown wait is introduced; the joined work remains bounded by the existing file/highlight limits.
