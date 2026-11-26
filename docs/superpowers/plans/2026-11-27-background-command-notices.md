# Background Command Notices Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A background shell command's completion reaches the model at its next step in the running turn. The notice carries the tail of the output, and commands the model already waited for, read to the end, or stopped are not announced again.

**Architecture:** `TerminalService` keeps its completion queue. The turn loop drains it beside `deliver_child_results`. The service's read, wait and stop paths mark completions observed. `queue_completion` reads the tail.

**Tech Stack:** Rust (orca-runtime): `terminal_service.rs`, `runtime_turn_loop.rs`, `runtime_normal_tool.rs`, `controller.rs`, `system_prompt.rs`.

**Spec:** `docs/superpowers/specs/2026-11-27-background-command-notices-design.md`

## Global Constraints

- Notices stay `<task-notification>` system messages. No new event type.
- They are delivered only when no tool calls are open, and each one at most once, also across a resume.
- Commit after each task; never push. No `TMPDIR`/`HOME` override; never touch `~/.orca`.
- Run node, npm and workspace-wide cargo test commands with `env -u NODE_OPTIONS`.

## Review Focus

- **Two commands finish during one model call.** Both notices are delivered at the next boundary, in completion order. Task 1 test `two_completions_are_delivered_at_the_next_boundary_in_order`.
- **A command whose output is under 8 KiB.** The notice holds all of it and no omission line. Task 2 test `short_output_is_delivered_whole`.
- **Output in multi-byte UTF-8 cut at the 8 KiB boundary.** The tail starts at a character boundary, never with a replacement character. Task 2 test `the_tail_starts_on_a_character_boundary`.
- **The model calls `task_wait`, and the command ends while the wait is running.** No notice follows. Task 3 test `a_completion_collected_by_task_wait_is_not_announced`.
- **A resumed session with a delivered notice.** After resume the notice is not appended again. Task 1 test `a_delivered_notice_is_not_repeated_after_resume`, or the nearest existing resume test for child results, extended.

---

### Task 1: Deliver shell completions at the turn's settled boundaries

**Files:**
- Modify: `crates/orca-runtime/src/runtime_turn_loop.rs`. Add `deliver_shell_completions` next to `deliver_child_results`, and call it at the same places.
- Modify: wherever the turn loop can reach the thread's `TerminalService`. The controller reads it as `ExtensionData::get::<TerminalService>()` from the thread extensions; give the turn loop the same handle.
- Test: the turn-loop or controller tests that already drive a background terminal. `controller.rs` has one around its `drain_terminal_notifications` tests.

**Interfaces:**
- Produces: `fn deliver_shell_completions<W: io::Write>(input: &mut RuntimeTurnLoopInput<'_, '_, W>) -> io::Result<bool>`. It returns whether anything was delivered.
  - It is a no-op while the conversation has open tool calls.
  - It appends each `TerminalCompletion::model_notification()` as a system message, persisted once, the way `deliver_child_results` persists child results.

- [ ] **Step 1: Write the failing tests:**
  - `a_completion_during_a_turn_reaches_the_next_model_request`: a scripted provider makes a first tool call that starts a background command (`yield_time_ms` short), then answers normally. The command finishes before the second model request. That request's messages contain the `<task-notification>` for the command.
  - `two_completions_are_delivered_at_the_next_boundary_in_order`.
  - `a_delivered_notice_is_not_repeated_after_resume`.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p orca-runtime --lib completion -- --test-threads=1`
Expected: the new tests FAIL. The notice appears only on the next thread turn.

- [ ] **Step 3: Implement `deliver_shell_completions`** and call it beside each `deliver_child_results` call.
- [ ] **Step 4: Run the tests to verify they pass.** Use the same command, plus `cargo test -p orca-runtime --lib terminal -- --test-threads=1`. Expected: PASS.
- [ ] **Step 5: Commit**

```bash
git add crates/orca-runtime/src
git commit -m "fix(runtime): a background command's end reaches the model in the same turn"
```

### Task 2: The notice carries the end of the output

**Files:**
- Modify: `crates/orca-runtime/src/terminal_service.rs` (`queue_completion`, `TerminalCompletion::model_notification`, tests).

**Interfaces:**
- Produces: `TerminalCompletion` gains `omitted_bytes: u64`, the bytes before the tail. `model_notification` appends, when `omitted_bytes > 0`: `\n[{omitted_bytes} earlier bytes not shown; read them with task_read_output using task_id {task_id} and cursor 0]`. This replaces today's `[completion output truncated]` line.

- [ ] **Step 1: Write the failing tests** in `terminal_service.rs`:
  - `the_notice_holds_the_tail_of_long_output`: the command prints `head-marker`, then 20 KiB of filler, then `tail-marker`. The notice contains `tail-marker`, does not contain `head-marker`, and contains `earlier bytes not shown`.
  - `short_output_is_delivered_whole`: a short output appears whole, with no omission line.
  - `the_tail_starts_on_a_character_boundary`: 9 KiB of `é`. The notice's output is valid UTF-8 with no `\u{FFFD}`.
- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p orca-runtime --lib terminal_service -- --test-threads=1`
Expected: FAIL.

- [ ] **Step 3: Implement.** Compute the start as `bytes_total.saturating_sub(COMPLETION_OUTPUT_MAX_BYTES)`, moved forward to a character boundary, and read from there.
- [ ] **Step 4: Run the tests to verify they pass.** Use the same command. Expected: PASS.
- [ ] **Step 5: Commit**

```bash
git add crates/orca-runtime/src/terminal_service.rs
git commit -m "fix(runtime): a finished command's notice shows the end of its output"
```

### Task 3: No notice for a command the model already dealt with; prompt wording

**Files:**
- Modify: `crates/orca-runtime/src/terminal_service.rs`. The read path at an explicit offset, the wait path, and the stop path mark the completion observed when they report a final status or when the model stopped the command.
- Modify: `crates/orca-runtime/src/runtime_normal_tool.rs`, if the wait or stop tools need to pass that through.
- Modify: `crates/orca-runtime/src/system_prompt.rs`. Change the "Long-running commands notify you…" line, and update the prompt tests that pin it.

- [ ] **Step 1: Write the failing tests:**
  - `a_completion_collected_by_task_wait_is_not_announced`.
  - `a_completion_read_at_an_explicit_cursor_is_not_announced`: `task_read_output` with `cursor` 0 after the command ended. Nothing is queued.
  - `a_command_the_model_stopped_is_not_announced`: `task_stop`, then drain. No notice.
  - A command that ends unobserved is still announced. Keep today's `notified` test.
  - In `system_prompt.rs` tests: the prompt contains `at your next step` and `task_wait`, and no longer contains `Long-running commands notify you when they finish`.
- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test -p orca-runtime --lib terminal_service -- --test-threads=1 && cargo test -p orca-runtime --lib system_prompt -- --test-threads=1`
Expected: FAIL.

- [ ] **Step 3: Implement** the observation rule and the new prompt line from the spec, Goal 4.
- [ ] **Step 4: Run the tests to verify they pass**

Run the Step 2 commands, then:

```bash
cargo fmt --all -- --check
cargo clippy -p orca-runtime --all-targets --locked
env -u NODE_OPTIONS node scripts/validate-runtime-surface-contract.mjs
```

Expected: every command exits 0, and clippy prints no warnings. If a prompt size pin or budget constant moves, update it here and report the old and new values.

- [ ] **Step 5: Commit**

```bash
git add crates/orca-runtime/src
git commit -m "fix(runtime): commands the model waited for or stopped are not announced again"
```
