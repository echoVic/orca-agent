# Repeat-Call Reminder and Parallel Network Reads Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement parts 1 and 2 of the spec:
1. A reminder when the model repeats the exact same tool call.
2. Consecutive network reads (`web_search`, `web_fetch`, `read_document`) run in parallel when no approval is needed.

Part 3, the tool-description guidance, is separate later work because it needs a prompt-budget decision. Don't implement it here.

**Architecture:**
- **The counter.** A per-thread repeat counter sits on the path where a completed tool result is appended. Its key is the tool name plus the arguments as canonical JSON, with keys sorted recursively.
- **The scheduler.** `runtime_tool_scheduler.rs` gains a network-read batch beside the read-only batch. A window forms only when the approval policy already allows every call in it.

**Tech Stack:** Rust (orca-runtime, orca-core `tool_types.rs`).

**Spec:** `docs/superpowers/specs/2026-11-27-tool-quality-design.md` (parts 1 and 2)

## Global Constraints

- **The reminder is advisory.** The repeated call always runs.
- **Reminder text, exactly.** At the 3rd identical consecutive call:

  `You have called {tool} with the same arguments {n} times in a row. The result will not change; use what you already have, change the arguments, or try a different approach.`

  At the 5th and 8th, the same sentence, followed by ` Arguments: {args cut at 300 characters}`.
- **The reminder message.**
  - It is appended as one system message, after that call's result.
  - It uses the existing system-message record, so Orca 0.5.24 reads it.
- **What resets the counter.** A different key resets the count to 1, and so does a new user message.
- **Excluded from counting.** `task_wait`, `task_read_output` and `task_list` neither count nor reset the counter. Calls inside a script are not counted.
- **Network batch.**
  - The batch takes calls whose capabilities are all read-only or `NetworkSearch`.
  - A window falls back to one-by-one when any call in it would need approval.
  - At most `tools.max_read_parallel` calls run at once.
  - Hooks run in model order before the batch, and results are recorded in model order.
- **Process.**
  - Commit after each task; never push.
  - Don't override `TMPDIR` or `HOME`, and never touch `~/.orca`.
  - Use `env -u NODE_OPTIONS` for node, npm and workspace-wide cargo tests.

## Review Focus

- **Same tool, same arguments, different key order.** These count as identical. Test `argument_key_order_does_not_change_the_key`.
- **A polling loop with `task_wait`.** No reminder. Test `task_wait_never_triggers_a_reminder`.
- **A batch in suggest mode.** In suggest mode network calls ask for approval, so the batch never forms and each call asks in order. Test `network_reads_that_need_approval_run_one_by_one`.
- **One of three parallel fetches fails.** The other two still complete, and all three results are recorded in model order. Test `a_failing_fetch_does_not_disturb_the_others_order`.
- **A web call and a write in the same window.** The batch stops at the write. Test `a_write_ends_the_network_batch`.

---

### Task 1: Repeat-call reminder

- [ ] **Step 1: Write the failing tests:**
  - `three_identical_calls_add_the_reminder`
  - `the_fifth_reminder_includes_the_arguments`
  - `a_different_call_resets_the_count`
  - `a_user_message_resets_the_count`
  - `argument_key_order_does_not_change_the_key`
  - `task_wait_never_triggers_a_reminder`
  - `the_reminder_survives_resume_as_a_system_message`
- [ ] **Step 2:** Run the tests and confirm they fail.
- [ ] **Step 3:** Implement the counter and the reminder.
- [ ] **Step 4:** Run `cargo test -p orca-runtime --lib repeat -- --test-threads=1` and confirm it passes.
- [ ] **Step 5:** Commit `feat(runtime): a reminder when the model repeats the exact same tool call`.

### Task 2: Parallel network reads

- [ ] **Step 1: Write the failing tests:**
  - `consecutive_web_fetches_run_in_parallel_when_allowed`: three fetches against a local delayed server. The total time is under twice one fetch's delay, and the results are in model order.
  - `network_reads_that_need_approval_run_one_by_one`
  - `a_failing_fetch_does_not_disturb_the_others_order`
  - `a_write_ends_the_network_batch`
  - `the_batch_respects_max_read_parallel`
- [ ] **Step 2:** Run the tests and confirm they fail.
- [ ] **Step 3:** Implement the batch in `runtime_tool_scheduler.rs`, reusing the read-only batch machinery.
- [ ] **Step 4:** Run `cargo test -p orca-runtime --lib scheduler -- --test-threads=1` and the named tests, and confirm they pass.
- [ ] **Step 5:** Commit `feat(runtime): network reads that need no approval run in parallel`.

### Final checks

- `env -u NODE_OPTIONS cargo test -p orca-runtime --lib -- --test-threads=1`
- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets --locked -j 4`
- `env -u NODE_OPTIONS node scripts/validate-runtime-surface-contract.mjs`
- `env -u NODE_OPTIONS node scripts/validate-windows-platform-boundaries.mjs`
- Update the site docs in English and Chinese: the tools page or the approval-modes page, wherever parallel calls are described.
