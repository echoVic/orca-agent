# Fork History Recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A damaged fork-history file fails only its own child, and a failed fork creation publishes nothing.

**Architecture:**
- **Reconciliation:** `read_record` stops resolving fork history, so registry reconciliation only validates and projects records.
- **Preparation:** fork history is resolved when a child is created, prepared or resumed.
- **Creation order:** `create_with_fork` loads the prefix before `create_record`.

**Tech Stack:** Rust, orca-runtime `crates/orca-runtime/src/agent_continuation.rs` (`read_record`, `read_fork_child`, `load_fork_prefix`, `create_record`, `create_with_fork`, `prepare_record`, `prepare_resume_with_parent_fence`) and `tasks.rs` (`TaskRegistry::new_persistent`).

**Spec:** `docs/superpowers/specs/2026-11-28-fork-history-recovery-design.md`

## Global Constraints

- **Formats.** Persist nothing new and keep every file format unchanged.
- **Existing behaviour that must keep working:**
  - refusing conflicting digests;
  - association-only recovery for records rewritten by an older release;
  - the PR #196 fixes, with their tests:
    - `a_missing_fork_association_uses_the_inline_reference`
    - `a_broken_fork_prefix_does_not_block_other_tasks`
    - `a_failed_fork_creation_does_not_bind_a_later_fresh_child`
    - `a_missing_fork_prefix_leaves_the_continuation_resumable`
- **Errors.** An error for a damaged file names the child's continuation ID and says which file could not be used. It never includes file contents.
- **Process.**
  - Commit after each task. Never push.
  - Do not override `TMPDIR` or `HOME`, and never touch `~/.orca`.
  - Use `env -u NODE_OPTIONS` for workspace-wide cargo test runs.

## Review Focus

- **Truncated association JSON for one child.** The registry opens, other tasks run, and only that child's resume fails with a clear error. Test: `a_corrupt_fork_association_does_not_block_the_registry`.
- **A prefix file removed between capture and creation.** Nothing is published, and a fresh creation with the same ID then succeeds. Test: `a_missing_prefix_blob_publishes_nothing`.
- **An older release removed the inline reference, and the association is intact.** The child still restores its prefix. Test: the existing association-only recovery test, run unchanged.
- **The inline reference and the association disagree.** Still refused at preparation, and the registry still opens. Test: `conflicting_fork_references_fail_only_that_child`.
- **Startup with many continuation records, one with a damaged prefix file.** Reconciliation reads no prefix file. Test: `reconciliation_reads_no_fork_history`, through a read counter or by making every prefix file unreadable.

---

### Task 1: Reconciliation does not resolve fork history

- [ ] **Step 1: Write the failing tests.**
  - `a_corrupt_fork_association_does_not_block_the_registry`
  - `a_corrupt_fork_association_fails_only_that_childs_resume`
  - `conflicting_fork_references_fail_only_that_child`
  - `reconciliation_reads_no_fork_history`
- [ ] **Step 2: Run them** and confirm they fail.
- [ ] **Step 3: Implement.** Move fork-history resolution out of `read_record` into the preparation paths. `read_record` returns the record with its references unresolved, and preparation resolves and validates them.
- [ ] **Step 4: Run** `env -u NODE_OPTIONS cargo test -p orca-runtime --lib -- --test-threads=1` and the subagent recovery contract tests, and confirm they pass.
- [ ] **Step 5: Commit** `fix(subagents): a damaged fork history file affects only its own child`.

### Task 2: A failed fork creation publishes nothing

- [ ] **Step 1: Write the failing tests.**
  - `a_missing_prefix_blob_publishes_nothing`: no record, no task projection and no association remain.
  - `a_fresh_retry_with_the_same_id_succeeds_after_a_failed_fork_creation`
- [ ] **Step 2: Run them** and confirm they fail.
- [ ] **Step 3: Implement.** Load and validate the prefix in `create_with_fork` before `create_record`, and carry the loaded `Arc` into the result.
- [ ] **Step 4: Run the tests** and confirm they pass.
- [ ] **Step 5: Commit** `fix(subagents): a fork that cannot load its history leaves nothing behind`.

### Task 3: Notes and checks

- [ ] **Release note.** Add an item to `docs/releases/unreleased.md`, creating it if needed.
- [ ] **Run:**
  - `cargo fmt --all -- --check`
  - `cargo clippy --workspace --all-targets --locked -j 4`
  - `env -u NODE_OPTIONS cargo test -p orca-runtime --lib -- --test-threads=1`
  - `env -u NODE_OPTIONS cargo test --test subagent_contract --test subagent_recovery_contract -- --test-threads=1`
  - `env -u NODE_OPTIONS node scripts/validate-runtime-surface-contract.mjs`
  - `env -u NODE_OPTIONS node scripts/validate-windows-platform-boundaries.mjs`
- [ ] **Commit** `docs: fork history recovery release note`.
