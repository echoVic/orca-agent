# TUI Queued Submission Aggregate Ownership Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give process-local TUI queued submissions one private aggregate owner while preserving FIFO dispatch, admission fencing, rollback, restoration, and preview behavior.

**Architecture:** `queued_input.rs` owns a private `QueuedSubmissionState` containing the deque, in-flight message, autosend flag, last error, and next id. `AppState` holds that one aggregate and keeps its current crate-local command methods for coordinating queue transitions with global status, transcript, history, and typed `UserAction`; UI consumes an owned bounded view.

**Tech Stack:** Rust 2024, Cargo, crossbeam bounded channels, ratatui, runtime-surface contract validators, PTY contract harness.

---

### Task 1: Establish The Owner-Level RED Behavior

**Files:** Modify `crates/orca-tui/src/queued_input.rs` tests only.

- [x] Add `failed_dispatch_restores_fifo_and_reports_error_atomically`. Construct two `QueuedUserMessage` values, enqueue them into `QueuedSubmissionState::default()`, begin the first, call `fail_dispatch("follow-up action queue is full")`, and assert: pending visible text is `["first", "second"]`, there is no in-flight id, autosend remains enabled, and the exact error is present.
- [x] Run `cargo test -p orca-tui failed_dispatch_restores_fifo_and_reports_error_atomically --lib --locked -- --test-threads=1` and confirm RED because `QueuedSubmissionState` and its transition API do not exist.

### Task 2: Implement The Private Queue Aggregate

**Files:** Modify `crates/orca-tui/src/queued_input.rs`.

- [x] Add private fields `pending: VecDeque<QueuedUserMessage>`, `in_flight: Option<QueuedUserMessage>`, `autosend: bool`, `error: Option<String>`, and `next_id: u64` to `pub(crate) struct QueuedSubmissionState`; implement `Default` with empty/no-fence/enabled/no-error/id-one state.
- [x] Implement owner transitions: `enqueue`, `pop_latest`, `begin_next`, `in_flight_prompt`, `rollback`, `take_rejected`, `suspend`, `resume_autosend`, `pending_or_in_flight`, `in_flight`, `matches_id`, `settle_started`, `fail_dispatch`, `report_error`, and `reset`. `fail_dispatch` must perform rollback and error assignment within the same mutable borrow.
- [x] Add `QueuedSubmissionView { preview, error }` and `view()` using the existing bounded `QueuedPreviewSnapshot`; do not expose the deque.
- [x] Add immutable `#[cfg(test)]` queries for pending text/binding counts, in-flight id, autosend, and error.
- [x] Re-run the focused RED test and the existing `queued_input::tests` suite; expect GREEN.

### Task 3: Replace AppState Fields And Move Coordination Methods

**Files:** Modify `crates/orca-tui/src/types.rs` and `crates/orca-tui/src/queued_input.rs`.

- [x] Replace the five AppState fields with `pub(crate) queued_submission: QueuedSubmissionState`, initialized with `QueuedSubmissionState::default()`.
- [x] Move the used queue AppState methods from `types.rs` into `queued_input.rs`. Delegate queue-only facts to the aggregate; keep `begin_next_queued_message` and `commit_queued_submission_admission` coordinating AppState status, transcript, input history, and typed actions, and replace the test-only standalone rollback caller with atomic `fail_queued_submission_dispatch`.
- [x] Add AppState intent/query methods `settle_queued_submission_started`, `fail_queued_submission_dispatch`, `report_queued_input_error`, `queued_submission_view`, and `queued_submission_in_flight`; add test-only immutable observations used by existing behavioral tests.
- [x] Replace reducer field clearing for `QueuedSubmissionStarted` with `settle_queued_submission_started(id)` and retain stale-id behavior.
- [x] Move the four core queued state-machine tests from the `types.rs` test module to `queued_input.rs`, adapting assertions to immutable behavior queries. Do not add source-shape assertions.
- [x] Run `cargo test -p orca-tui queued_ --lib --locked -- --test-threads=1`; expect all focused behavior tests to pass.

### Task 4: Remove External Direct Fact Access

**Files:** Modify `crates/orca-tui/src/queued_input_actions.rs`, `crates/orca-tui/src/ui.rs`, and tests in `app.rs`, `global_actions.rs`, `idle_submit_actions.rs`, `plan_approval_actions.rs`, `running_actions.rs`, `runtime_event_actions.rs`, `status_key_actions.rs`, and `types.rs`.

- [x] In dispatch, replace separate rollback/error writes with `fail_queued_submission_dispatch` and capacity error writes with `report_queued_input_error`; preserve exact displayed strings and `QueuedDispatch` results.
- [x] In UI, replace deque/error reads with `queued_submission_view()`; preserve row count, head/second/tail sampling, error color, and exact labels.
- [x] Convert tests to public behavior/query methods. Setup that previously assigned an in-flight field must enqueue and begin normally; no mutable aggregate access may be added for tests.
- [x] Run `rg -n "queued_user_messages|queued_submission_in_flight|queued_follow_up_autosend|queued_input_error|next_queued_submission_id" crates/orca-tui/src -g '*.rs'`. Expected: no obsolete AppState field accesses; matches may only be method names deliberately retained for compatibility.
- [x] Run `cargo test -p orca-tui queued_input --lib --locked -- --test-threads=1` and focused action/UI suites; expect GREEN.

### Task 5: Synchronize Contracts, Documentation, And Full Gates

**Files:** Modify `scripts/validate-runtime-surface-contract.mjs`, `docs/production-roadmap.md`, and this spec/plan. Modify the reviewed manifest/digest only if the validator reports reviewed entrypoint drift.

- [x] Relocate `commit_queued_submission_admission:input_history.record` from `types.rs` to `queued_input.rs`; remove the obsolete `reset_queued_user_messages:self.queued_user_messages.clear` harmless-site baseline because aggregate reset has no collection-clear call.
- [x] Update the roadmap with the aggregate owner, channel/global-state boundaries, process-local restart contract, and unchanged external protocols. Mark the spec implemented and plan checkboxes complete only after evidence exists.
- [x] Run `cargo test -p orca-tui --lib --locked -- --test-threads=1` and `cargo test --test tui_pty_contract --locked -- --test-threads=1`.
- [x] Run both Node validator suites, `cargo fmt --all -- --check`, `git diff --check`, and the obsolete-field search. Every command must exit zero.
- [x] Request independent review focused on ownership leaks, FIFO/fence failure behavior, stale-event handling, preview compatibility, and missing tests; address all Critical and Important findings.

### Task 6: Rebase, Verify, And Commit

**Files:** All slice files.

- [ ] Fetch and rebase the branch on latest local `main`/`origin/main` without touching unrelated worktrees. Rerun the owner focused test, TUI full suite, PTY suite, both Node validators, formatting, and diff checks.
- [ ] Review `git diff main...HEAD` plus uncommitted changes and confirm no second queue cache, direct fact mutation, compatibility wrapper, protocol change, persistence change, or unrelated cleanup.
- [ ] Commit once with `refactor(tui): give queued submissions one state owner`.
- [ ] Use `superpowers:finishing-a-development-branch`; if local main is clean and still the verified base, fast-forward locally, re-run affected gates on main, then remove only this owned worktree/branch. Do not push or publish this architecture-only slice.

## Plan Self-Review

Every spec transition maps to Tasks 1-4; deletion, docs, validators, review, rebase, and integration map to Tasks 5-6. The aggregate owns only queue facts and does not absorb channel, transcript, runtime operation, or persistence ownership. No placeholder, second fact source, source-shape behavioral test, or long-lived compatibility path is introduced.
