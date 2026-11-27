# Multi-Agent Steering, Delegation Guidance and Workflow Task Lists Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement parts 1–3 of the spec:
1. A user steer reaches a parent that is waiting for its children.
2. The prompt tells the model it may end its reply while children run.
3. Workflow task lists keep every update, check claimants, and release tasks whose claimant died.

Part 4 (fork children) is separate later work. Don't implement it here.

**Architecture:** The order of work comes from the spec: reproduce first, then fix only what fails.
- Part 1 touches the wait loops in `runtime_normal_tool.rs` (`task_wait`) and `runtime_turn_loop.rs` (end-of-turn child waits). It also touches the steer reader in `runtime_steer.rs`.
- Part 2 is one sentence in the Delegation section in `agent_common.rs`.
- Part 3 touches `workflow/host.mjs`, `workflow/runner.rs`, `workflow/host.rs` and `workflow/ipc.rs`.

**Tech Stack:** Rust (orca-runtime) and the Node workflow host (`workflow/host.mjs`).

**Spec:** `docs/superpowers/specs/2026-11-27-multi-agent-design.md` (parts 1–3)

## Global Constraints

- Prompt text changes must keep the prompt within its size tests. If the Delegation sentence pushes the prefix over its ceiling, first tighten the sentence. Only if that fails, raise the ceiling to the smallest round value that fits, and report the old and new values.
- Nothing new is written into sessions in a form Orca 0.5.24 cannot read (closed enums, `deny_unknown_fields` on `SessionMeta`).
- Commit after each task and never push. Don't override `TMPDIR`/`HOME`, and never touch `~/.orca`. Use `env -u NODE_OPTIONS` for node, npm and workspace-wide cargo tests.

## Review Focus

- **Two steers in a row during a wait.** Both are answered in order, and the wait resumes afterwards if the model asks. Test `two_steers_during_a_wait_are_answered_in_order`.
- **A steer while no wait is running.** Behaviour is unchanged. Test `a_steer_outside_a_wait_is_unchanged`.
- **A workflow child that completes a task it never claimed.** The call is refused with a clear error, and the task is unchanged. Test `completing_an_unclaimed_task_is_refused`.
- **Two children claiming at the same instant.** Exactly one wins, and the other gets the next pending task or none. Test `concurrent_claims_do_not_share_a_task`.
- **A claimant that crashes.** Its running task returns to `pending`, and another child can claim it. Test `a_crashed_claimant_releases_its_task`.

---

### Task 1: A steer ends a wait for children

- [ ] **Step 1:** Write the reproduction tests, using the runtime's scripted-provider helpers and a long-running child:
  - `a_steer_during_task_wait_is_answered_before_the_child_finishes`
  - `a_steer_during_the_end_of_turn_child_wait_is_answered`
- [ ] **Step 2:** Run them.
  - If they already pass, report that the problem does not exist, keep the tests, skip Step 3, and commit `test(runtime): a steer reaches a parent waiting for its children`.
- [ ] **Step 3:** Otherwise, make both waits also end when steering input is pending.
  - The next model step answers the steer.
  - The model may wait again afterwards.
  - Add `two_steers_during_a_wait_are_answered_in_order` and `a_steer_outside_a_wait_is_unchanged`.
- [ ] **Step 4:** Run `cargo test -p orca-runtime --lib steer -- --test-threads=1`, then `cargo test -p orca-runtime --lib task_wait -- --test-threads=1`. Both must pass.
- [ ] **Step 5:** Commit `fix(runtime): a steer reaches a parent that is waiting for its children`.

### Task 2: The prompt says the model may end its reply while children run

- [ ] Add to the Delegation section (`agent_common.rs`), in substance: "When only waiting for subagent results remains, end your reply instead of polling; Orca keeps the turn open and continues it with each result as it arrives."
- [ ] Assert the sentence in the prompt tests, and keep the budget tests passing.
- [ ] Commit `fix(prompt): the model may end its reply while subagents run`.

### Task 3: Workflow task lists keep every update

- [ ] **Step 1:** Write the reproduction tests:
  - `updates_from_the_host_and_the_runner_are_both_kept`: a lost update between the two writers.
  - `completing_an_unclaimed_task_is_refused`
  - `concurrent_claims_do_not_share_a_task`
  - `a_crashed_claimant_releases_its_task`
- [ ] **Step 2:** Run them and note which fail.
- [ ] **Step 3:** Fix each failing one:
  - One side becomes the only writer of `task-lists.json`, and the other asks it.
  - `complete` requires that the caller is the claimant and the task is `running`.
  - A claimant that stops or fails releases its task to `pending`.
- [ ] **Step 4:** Run `cargo test -p orca-runtime --lib workflow -- --test-threads=1`, plus any workflow integration tests (`tests/` files named for workflows). All must pass.
- [ ] **Step 5:** Commit `fix(workflow): task lists keep every update and check who completes a task`.

### Final checks

Run each and confirm it passes:
- `env -u NODE_OPTIONS cargo test -p orca-runtime --lib -- --test-threads=1`
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets --locked -j 4`
- `env -u NODE_OPTIONS node scripts/validate-runtime-surface-contract.mjs`
- `env -u NODE_OPTIONS node scripts/validate-windows-platform-boundaries.mjs`
