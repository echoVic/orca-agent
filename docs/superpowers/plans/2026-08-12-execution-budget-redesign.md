# Execution Budget Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace Orca's hidden 128-turn ceiling and fragmented budget handling with one explicit execution-budget protocol supporting natural completion, typed termination, durable checkpoints, child-agent leases, and resumable headless execution.

**Architecture:** A session owns history, an operation owns one user request, and each operation owns one `BudgetController`. Model requests, tools, child agents, workflows, and Goal continuations consume typed leases. One append-only execution journal is the source of truth; TUI, JSONL, Harbor, and resume state are projections committed after the journal checkpoint is durable.

**Tech Stack:** Rust workspace (`orca-core`, `orca-runtime`, `orca-tui`), serde/JSONL, existing SessionWriter/runtime-surface ledger, clap CLI, Python Harbor adapter, Cargo integration tests.

---

## Non-Compatibility Decisions

This redesign intentionally breaks the existing runtime/public contracts:

- Delete `DEFAULT_MAX_TURNS = 128`; no implicit turn limit exists.
- Delete `RunStatus::BudgetExhausted` and `TurnEndReason::MaxInnerTurns`.
- Replace status-plus-reason pairs with one typed `OperationTerminal`.
- Replace `max_budget_usd` with `[budget]` configuration and explicit CLI options.
- Make the execution journal the only authority for terminal/checkpoint facts.
- Never replay a `tool.started` without a committed `tool.completed`; restore it as `indeterminate`.
- Never convert verifier success into operation success after a budget/safety stop.

## New Contract

```text
Session
  └── Operation
        ├── BudgetController
        ├── Turn 1..N (unbounded unless explicitly limited)
        │     └── ToolAttempt 1..M
        ├── Checkpoint 0..N
        └── OperationTerminal
```

```rust
pub struct BudgetSpec {
    pub max_turns: Option<u32>,
    pub max_tool_calls: Option<u32>,
    pub max_cost_usd_micros: Option<u64>,
    pub max_wall_time_ms: Option<u64>,
}

pub struct BudgetUsage {
    pub turns: u32,
    pub tool_calls: u32,
    pub cost_usd_micros: u64,
    pub wall_time_ms: u64,
}

pub enum OperationTerminal {
    Completed { usage: BudgetUsage },
    Stopped { reason: StopReason, usage: BudgetUsage, checkpoint_id: String, resumable: bool },
    Failed { class: FailureClass, message: String },
    Cancelled { reason: CancelReason, checkpoint_id: Option<String> },
}
```

`BudgetSpec` dimensions are independently optional. `ModelEnded` is normal completion. Budget and safety stops are non-success terminals. `resumable` is true only after a committed conversation boundary exists.

## File Ownership Map

- Create `crates/orca-core/src/budget.rs`: budget, usage, stop, terminal types.
- Modify `crates/orca-core/src/config/mod.rs` and `config/file.rs`: replace legacy budget fields with `BudgetConfig`.
- Create `crates/orca-runtime/src/budget_controller.rs`: admission, accounting, reminders, child leases.
- Create `crates/orca-runtime/src/execution_journal.rs`: ordered facts and atomic flush.
- Modify `agent_loop.rs`, `lifecycle.rs`, `runtime_turn_loop.rs`, `runtime_turn_iteration.rs`: inject controller and remove constant checks.
- Modify `runtime_host.rs`, `thread.rs`, `runtime_surface/*`, `event_schema.rs`: publish one typed terminal.
- Modify `goal_actor.rs`, `goal_store.rs`, child-agent and workflow modules: hierarchical budgets and leases.
- Modify `src/cli.rs`, `command/exec.rs`, TUI status/event modules, and `terminal_bench/orca_agent.py`.
- Delete obsolete 128-turn fixtures/specs after replacement tests pass.

## Task 1: Freeze the Contract

**Files:** `docs/superpowers/specs/2026-08-12-execution-budget-redesign.md`, `tests/execution_budget_contract.rs`

- [x] Write failing tests for unlimited default, typed budget stop, checkpoint-before-terminal ordering, and verifier-success/operation-stop separation.
- [x] Run `cargo test --test execution_budget_contract --locked`; expect compile failure because new types do not exist.
- [x] Document operation/turn/tool/checkpoint boundaries, child leases, restore semantics, and the rule that external side effects are not replayed.
- [x] Review against Codex rollout persistence, Grok explicit `--max-turns`, and Claude Code pre-result flush. Preserve invariants, not their wire formats.

## Task 2: Add Core Budget Types

**Files:** `crates/orca-core/src/budget.rs`, `crates/orca-core/src/lib.rs`, `crates/orca-core/src/config/mod.rs`, `crates/orca-core/src/config/file.rs`, `crates/orca-core/src/budget_tests.rs`

- [x] Add pure types and tests for unlimited dimensions, positive-value validation, saturating usage, and stable snake-case serialization.
- [x] Add `BudgetConfig` with `max_turns`, `max_tool_calls`, `max_cost_usd_micros`, and `max_wall_time_ms`; default all to `None`.
- [x] Remove `RunConfig::max_budget_usd` and legacy config aliases.
- [x] Run `cargo test -p orca-core --lib budget --locked` and `cargo fmt --all -- --check`.

## Task 3: Implement `BudgetController`

**Files:** `crates/orca-runtime/src/budget_controller.rs`, `crates/orca-runtime/src/lib.rs`, `crates/orca-runtime/src/budget_controller_tests.rs`

- [x] Write RED tests for unlimited turns, turn/tool/cost/wall-time exhaustion, usage accounting, reminders, and child lease bounds.
- [x] Implement `admit_turn`, `admit_tool_call`, `record_usage`, `child_lease`, and `terminal`.
- [x] Reserve parent budget before spawning a child; return unused reservations and always report consumed usage.
- [x] Move soft-landing reminders into the controller; reminders never mutate usage or success state.
- [x] Run `cargo test -p orca-runtime --lib budget_controller --locked`.

## Task 4: Make the Journal Authoritative

**Files:** `crates/orca-runtime/src/execution_journal.rs`, `session.rs`, `thread_store/writer.rs`, `crates/orca-runtime/tests/execution_journal.rs`

- [x] Write failure-injection tests proving `tool.completed` is durable before `checkpoint.created`, and checkpoint before `operation.terminal`.
- [x] Implement ordered records: `operation.started`, `turn.started`, `model.response`, `tool.started`, `tool.completed`, `checkpoint.created`, `operation.terminal`.
- [x] Put `operation_id`, `turn_id`, ordinal, and schema version on every record.
- [x] Feed JSONL and transcript projections from committed journal records; neither projection may invent terminal facts.
- [x] Run `cargo test -p orca-runtime --test execution_journal --locked -- --test-threads=1`.

## Task 5: Remove the Hidden Turn Ceiling

**Files:** `agent_loop.rs`, `lifecycle.rs`, `runtime_turn_loop.rs`, `runtime_turn_iteration.rs`, `tests/agent_loop_contract.rs`

- [x] Replace the old 128-turn fixture with an unlimited mock run over 128 tool cycles and an explicit three-turn budget run.
- [x] Construct one controller per operation and pass it through the loop; delete `DEFAULT_MAX_TURNS`, `RuntimeTaskActor.max_turns`, and constant comparisons.
- [x] On budget stop, settle the current committed tool, create a checkpoint, and return `OperationTerminal::Stopped` without another provider request.
- [x] Run `cargo test --test agent_loop_contract --locked -- --test-threads=1`.

## Task 6: Replace Status/Reason Plumbing

**Files:** `event_schema.rs`, `runtime_surface/operation.rs`, `runtime_surface/projection.rs`, `runtime_host.rs`, `thread.rs`, `goal_actor.rs`, `crates/orca-runtime/tests/operation_terminal_contract.rs`

- [x] Delete `RunStatus::BudgetExhausted` and `TurnEndReason::MaxInnerTurns`; map all callers to typed terminals.
- [x] Make every surface consume the same terminal object; adapters must not reconstruct limits from constants.
- [x] Test terminal ordering and independent verification metadata.
- [x] Run `cargo test -p orca-runtime --test operation_terminal_contract --locked`.

## Task 7: Make Resume and Goals Budget-Correct

**Files:** `runtime_host.rs`, `goal_actor.rs`, `goal_store.rs`, `session.rs`, `crates/orca-runtime/tests/budget_resume_contract.rs`

- [x] Test resume from a budget checkpoint, interrupted-tool restore, refusal to replay indeterminate tools, and fresh operation id/budget.
- [x] Resume from `last_committed_message_id`; insert an indeterminate result for any unmatched `tool.started`.
- [x] Make Goal own cumulative budget; each continuation obtains a child operation lease. Exhausted Goal budget disables automatic continuation.
- [x] Run `cargo test -p orca-runtime --test budget_resume_contract --locked -- --test-threads=1`.

## Task 8: Bound Child Agents and Workflows

**Files:** `child_agent_loop_runner.rs`, `subagent_execution.rs`, `workflow/runner.rs`, `agent_child.rs`, `crates/orca-runtime/tests/budget_lease_contract.rs`

- [x] Test child limits, unused lease return, failed-child usage receipts, and detached background operation isolation.
- [x] Pass `BudgetLease` through child contexts; remove child construction of a global max-turn actor.
- [x] Charge synchronous children to the parent operation; require independent budgets for detached background work.
- [x] Run `cargo test -p orca-runtime --test budget_lease_contract --locked -- --test-threads=1`.

## Task 9: Replace CLI and Surface Protocols

**Files:** `src/cli.rs`, `crates/orca-runtime/src/command/exec.rs`, TUI status/event modules, `terminal_bench/orca_agent.py`, `tests/exec_jsonl.rs`, `terminal_bench/test_orca_agent.py`

- [x] Add `--max-turns`, `--max-tool-calls`, `--max-cost-usd`, and `--max-wall-time-secs`; interactive mode is unlimited unless explicitly bounded or cancelled.
- [x] Emit `operation.started`, `budget.warning`, `checkpoint.created`, and `operation.terminal`; delete legacy budget-terminal reconstruction.
- [x] Render usage, limits, stop reason, and checkpoint/resume state from the typed terminal.
- [x] Wrap Harbor execution in `try/finally`; always persist stdout, stderr, exit code, terminal metadata, and raw `trajectory.jsonl` on non-zero exits.
- [x] Run `cargo test --test exec_jsonl --locked -- --test-threads=1` and `python -m unittest terminal_bench.test_orca_agent`.

## Task 10: Delete Legacy Paths and Rewrite Docs

**Files:** `docs/harness-contract.md`, `docs/production-roadmap.md`, `README.md`, `README.zh-CN.md`, superseded 128-turn specs/fixtures

- [x] Run `rg -n "DEFAULT_MAX_TURNS|MaxInnerTurns|max_budget_usd|RunStatus::BudgetExhausted|max 128 turns" crates src tests docs terminal_bench` and remove every non-historical match.
- [x] Document unlimited natural completion, explicit budgets, typed terminals, checkpoint ordering, child leases, and indeterminate side effects.
- [x] Remove documentation that treats 128 turns as the current product contract.
- [x] Run `cargo fmt --all -- --check` and `git diff --check`.

## Task 11: Full Verification

- [x] Run `cargo test -p orca-core --lib --locked`.
- [x] Run `cargo test -p orca-runtime --lib --locked`.
- [x] Run `cargo test -p orca-runtime --tests --locked -- --test-threads=1`.
- [x] Run `cargo test --workspace --locked`.
- [x] Verify unlimited >128-turn mock execution, explicit turn/cost/time stops, checkpoint resume, and no replay of indeterminate tools.
- [x] Run a credentialed Terminal-Bench sample and record binary revision, budget spec, exit code, checkpoint id, trajectory presence, and verifier result.
- [x] Review the final diff for duplicate budget sources, second trajectory writers, hidden caps, compatibility shims, or automatic external-side-effect replay.

## Completion Criteria

- No production fixed 128-turn ceiling.
- Unlimited execution is the default unless a caller supplies a budget.
- One `BudgetController` owns all operation limits; children use leases.
- Checkpoint durability precedes terminal publication.
- Resume creates a new operation and never silently replays unresolved external effects.
- TUI, JSONL, history, Goal, and Harbor agree on one terminal record.
- Budget stop, verifier result, and process exit are independently observable.
- Focused tests, workspace tests, and real headless evidence pass.
