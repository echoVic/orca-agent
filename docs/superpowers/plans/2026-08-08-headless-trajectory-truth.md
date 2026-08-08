# Headless Trajectory Truth Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Prove that headless `orca exec` reports and persists the exact default 128-turn safety boundary.

**Architecture:** Extend only the deterministic mock provider fixture, then exercise the existing controller, event sink, and session writer through the real `orca exec` binary. No runtime loop or protocol owner changes are required because the current `RuntimeTaskActor` already owns the admission decision.

**Tech Stack:** Rust, Cargo integration contracts, JSONL event/transcript inspection.

---

### Task 1: Add the RED headless behavior contract

**Files:**
- Modify: `tests/agent_loop_contract.rs`

- [x] **Step 1: Add `headless_max_inner_turns_preserve_trajectory_truth`.**

The test runs the built `orca` binary with a temporary `ORCA_HOME`,
`--output-format jsonl`, `--save-history`, provider `mock`, and prompt
`mock_repeat_read 256`. It must assert exit code `4`, exactly 128 `turn.started`
events, exactly 128 `tool.call.completed` events, one final
`session.completed` with `budget_exhausted`, and no event after that terminal.
It then loads the sole saved session JSONL and asserts exactly 128 tool
conversation messages, each with the existing flattened terminal metadata
(`status=completed`, `kind=success`, and `exit_code=0`), and no 129th tool call
id.

- [x] **Step 2: Run the focused test and confirm RED.**

Run:

```bash
cargo test --test agent_loop_contract headless_max_inner_turns_preserve_trajectory_truth -- --exact --nocapture
```

Observed RED: before the fixture branch, the real binary returned exit code 0
for `mock_repeat_read 256`, so the requested budget terminal was missing.

### Task 2: Implement the deterministic repeated-turn fixture

**Files:**
- Modify: `crates/orca-provider/src/lib.rs`

- [x] **Step 1: Add a prompt parser for `mock_repeat_read <count>`.**

Parse a positive integer count, cap it at 256 for bounded tests, and return a
`ReadFile` request with a stable id derived from the current completed-tool
count. Keep this branch before the generic `has_tool_results` final-response
branch so the fixture can request another tool after each result. Once the
requested count is reached, return the existing success message.

- [x] **Step 2: Run the focused test and confirm GREEN.**

Run the exact command from Task 1 and verify Cargo exits 0 with the asserted
process exit code 4 and 128/128/one-terminal counts.

- [x] **Step 3: Run the provider fixture unit tests.**

Run:

```bash
cargo test -p orca-provider mock_repeat_read --lib
```

Add a small parser unit test if the existing provider test module does not
already exercise the new prompt branch.

### Task 3: Document and validate the slice

**Files:**
- Modify: `docs/production-roadmap.md`
- Modify: `docs/superpowers/specs/2026-08-08-headless-trajectory-truth.md`
- Modify: `docs/superpowers/plans/2026-08-08-headless-trajectory-truth.md`

- [x] **Step 1: Update the current roadmap baseline.**

Recorded the real-binary 128-turn JSONL/transcript contract and its unchanged
runtime ownership in `docs/production-roadmap.md`.

- [x] **Step 2: Record final evidence in Spec and Plan.**

Recorded the RED result, fixture behavior, and the existing flattened terminal
wire fields without changing the persistence schema.

- [x] **Step 3: Run all required verification commands.**

```bash
cargo test --test agent_loop_contract -- --test-threads=1
cargo test --test runtime_lifecycle_contract -- --test-threads=1
cargo fmt --all -- --check
git diff --check
```

Evidence: the serial agent-loop contract passed 4 tests (including the
real-binary boundary run in 51.96s), the serial runtime lifecycle contract
passed 54 tests, the provider fixture tests passed 2 tests, and formatter and
diff checks passed.

### Task 4: Review, commit, rebase, and re-verify

- [x] **Step 1: Review the full diff for protocol and ownership drift.**

Confirm only the provider fixture, behavior contract, roadmap, Spec, and Plan
changed; there is no new loop, writer, event type, or compatibility store.

- [x] **Step 2: Create one semantic commit.**

```bash
git add crates/orca-provider/src/lib.rs tests/agent_loop_contract.rs docs/production-roadmap.md docs/superpowers/specs/2026-08-08-headless-trajectory-truth.md docs/superpowers/plans/2026-08-08-headless-trajectory-truth.md
git commit -m "test(headless): prove max-turn trajectory truth"
```

- [x] **Step 3: Rebase `origin/main` and rerun focused/full gates.**

Run `git fetch origin main`, rebase the feature branch if the remote advanced,
then rerun the focused agent-loop test, serial lifecycle contract, formatter,
and diff check before delivery.

Post-commit evidence: `origin/main` was current with no rebase conflicts;
the focused contract passed in 51.10s, the provider fixture tests passed 2,
the serial lifecycle contract passed 54, and formatter and diff checks passed.
