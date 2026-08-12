# Execution Budget Redesign Spec

## Problem and Evidence

Orca currently hides a fixed 128-turn ceiling behind `DEFAULT_MAX_TURNS` in
`crates/orca-runtime/src/agent_loop.rs` (and a second copy in
`crates/orca-runtime/src/controller.rs`). The ceiling is enforced by
`RuntimeTaskActor.start_turn`, which compares `turns_started >= max_turns`
against a constant, producing `RunStatus::BudgetExhausted` paired with
`TurnEndReason::MaxInnerTurns`. Cost limits live in a separate
`max_budget_usd: Option<f64>` config field checked by the cost tracker and by
ad-hoc callers (`subagent_budget_exhaustion_error` in `tool_turn.rs`), and
soft-landing reminders are split between `RuntimeTaskActor` inner-turn state
and `CostTracker` cost state (`budget_soft_landing.rs`). Terminal facts are
reconstructed by adapters (TUI `surface_client.rs`, `surface_projection.rs`,
Harbor `orca_agent.py`) from status-plus-reason pairs instead of one typed
record.

This fragmented design means:

- There is no single owner of "what limits this operation" — the loop
  hard-codes a limit, the config carries a second limit, and the goal tracker
  reinvents a third.
- Checkpoints, tool settlement, and terminal publication have no enforced
  durability ordering; a crash between `tool.completed` persistence and
  `checkpoint.created` can replay external side effects on resume.
- Adapters re-derive budget semantics from string statuses, so a new budget
  dimension (tool calls, wall time) requires adapter changes in lockstep.

## User and Architecture Value

Users and headless consumers (Terminal-Bench, Harbor) get an explicit budget
protocol: unlimited by default, independently optional dimensions
(`max_turns`, `max_tool_calls`, `max_cost_usd_micros`, `max_wall_time_ms`),
typed terminals that survive the TUI/JSONL/Harbor boundary unchanged, and
durable checkpoints that make resume safe. Goals and child agents share one
accounting model via child leases instead of each subsystem owning a private
cap.

## Scope

In scope:

- `orca-core` budget types: `BudgetSpec`, `BudgetUsage`, `OperationTerminal`,
  `StopReason`, `FailureClass`, `CancelReason` (one typed terminal).
- `BudgetConfig` in `RunConfig` replacing `max_budget_usd`; explicit CLI
  options for each dimension.
- `BudgetController` in `orca-runtime` owning admission, accounting, reminders,
  and child leases; one controller per operation.
- `ExecutionJournal`: ordered records (`operation.started`, `turn.started`,
  `model.response`, `tool.started`, `tool.completed`, `checkpoint.created`,
  `operation.terminal`) with atomic flush; JSONL/transcript projections feed
  only from committed records.
- Removal of `DEFAULT_MAX_TURNS`, `RunStatus::BudgetExhausted`,
  `TurnEndReason::MaxInnerTurns`, and `RuntimeTaskActor.max_turns`.
- Resume from `last_committed_message_id`; unmatched `tool.started` restored as
  indeterminate; never replay committed external effects.
- Goals own cumulative budget; each continuation is a child-operation lease.
- Child agents and workflows consume `BudgetLease` reservations.

Out of scope:

- Provider pricing changes; `CostTracker` stays the cost estimator, but its
  limit checks move into the controller.
- Wire formats of Codex/Grok/Claude Code budgets; we preserve invariants
  (durable checkpoint before terminal, explicit limits, pre-result flush) not
  their JSON shapes.
- TUI visual redesign beyond rendering the typed terminal.

## Contract

```text
Session
  └── Operation
        ├── BudgetController
        ├── Turn 1..N (unbounded unless explicitly limited)
        │     └── ToolAttempt 1..M
        ├── Checkpoint 0..N
        └── OperationTerminal
```

- `BudgetSpec` dimensions are independently optional; a dimension with `None`
  is unlimited. `ModelEnded` is normal completion. Budget and safety stops are
  non-success terminals.
- `resumable` is true only after a committed conversation boundary exists
  (`checkpoint.created` durable before `operation.terminal`).
- Never replay a `tool.started` without a committed `tool.completed`; restore
  it as `indeterminate`.
- Never convert verifier success into operation success after a budget/safety
  stop; budget stop, verifier result, and process exit are independently
  observable.
- A `BudgetLease` reserves parent budget before a child spawns; unused
  reservations return to the parent; consumed usage always reports upward.

## Lifecycle and Failure Semantics

- Turns admit through the controller; each `admit_turn`/`admit_tool_call`
  increments usage and checks all bounded dimensions.
- On the first exhausted dimension the controller returns a typed stop; the
  loop settles the current committed tool, creates a checkpoint, and returns
  `OperationTerminal::Stopped` without issuing another provider request.
- Wall-time is measured from operation start; cost is accounted from provider
  usage deltas (`CostTracker` estimates converted to micros).
- Soft-landing reminders are pure policy emitted by the controller; they never
  mutate usage or success state.
- On crash, the journal is the only authority: committed records replay in
  order; uncommitted records are discarded or restored as indeterminate;
  external side effects are never replayed.

## Ownership and Boundaries

- `orca_core::budget` owns the pure types and validation (serde snake_case).
- `orca_runtime::budget_controller` owns admission/accounting/reminders/leases.
- `orca_runtime::execution_journal` owns ordered facts and atomic flush;
  `SessionWriter` and the JSONL emitter are projections of committed records.
- `RuntimeTaskActor` keeps lifecycle/turn-start bookkeeping but drops the
  max-turn constant; `CostTracker` keeps estimation only.
- `goal_actor`/`goal_store` own cumulative Goal budget; child-agent and
  workflow runners own lease consumption.

## Compatibility

Breaking by design: `max_budget_usd` config key is replaced by `[budget]`;
`RunStatus::BudgetExhausted` and `TurnEndReason::MaxInnerTurns` are deleted;
CLI gains `--max-turns`, `--max-tool-calls`, `--max-cost-usd`,
`--max-wall-time-secs`. `exec --output-format jsonl` keeps `session.started`
and `session.completed` envelopes but the terminal payload carries the typed
terminal; adapters consume the typed object. Interactive mode stays unlimited
unless explicitly bounded or cancelled.

## Acceptance Criteria

1. A `BudgetController` with default spec admits more than 128 turns and tool
   calls; no implicit ceiling exists.
2. A bounded controller stops on the first exhausted dimension with the typed
   reason; `OperationTerminal::Stopped` carries usage, checkpoint id, and
   `resumable` only after a committed checkpoint.
3. Journal ordering is enforced: `tool.completed` durable before
   `checkpoint.created` before `operation.terminal`; failure injection proves
   no terminal publishes before its checkpoint is durable.
4. Verifier success never upgrades a budget stop; exit code and terminal are
   independently observable.
5. Child leases reserve parent budget, return unused reservations, and report
   consumed usage; detached operations require their own budget.
6. Legacy symbols (`DEFAULT_MAX_TURNS`, `MaxInnerTurns`, `max_budget_usd`,
   `RunStatus::BudgetExhausted`) have no production references.
7. `cargo test --workspace --locked`, formatting, and diff checks pass; a
   credentialed Terminal-Bench sample records budget spec, exit code,
   checkpoint id, trajectory presence, and verifier result.

## Final Evidence

- RED contract test (`tests/execution_budget_contract.rs`) fails to compile
  before the new types exist, then passes with the implementation.
- `cargo test -p orca-core --lib budget`, `-p orca-runtime --lib
  budget_controller`, `-p orca-runtime --test execution_journal`,
  `--test operation_terminal_contract`, `--test budget_resume_contract`,
  `--test budget_lease_contract`, `--test agent_loop_contract`,
  `--test exec_jsonl` all pass with `--locked`.
- `rg -n "DEFAULT_MAX_TURNS|MaxInnerTurns|max_budget_usd|RunStatus::BudgetExhausted|max 128 turns" crates src tests docs terminal_bench` returns only historical/changelog matches.
- Credentialed real-API sample (binary `orca 0.3.15`, `--max-turns 1
  --max-cost-usd 0.01`): exit code 4, `session.completed` status
  `budget_exhausted` carrying `{"stopped": {"checkpoint_id":
  "run-...-budget-stop", "reason": {"turn_budget": {"max_turns": 1}},
  "resumable": true, ...}}`; trajectory streamed as JSONL. A bounded
  natural-completion sample exited 0 with the typed terminal absent (plain
  success), keeping budget stop, verifier result, and process exit
  independently observable.

## Verification Commands

```bash
cargo test --test execution_budget_contract --locked
cargo test -p orca-core --lib budget --locked
cargo test -p orca-runtime --lib budget_controller --locked
cargo test -p orca-runtime --test execution_journal --locked -- --test-threads=1
cargo test -p orca-runtime --test operation_terminal_contract --locked
cargo test -p orca-runtime --test budget_resume_contract --locked -- --test-threads=1
cargo test -p orca-runtime --test budget_lease_contract --locked -- --test-threads=1
cargo test --test agent_loop_contract --locked -- --test-threads=1
cargo test --test exec_jsonl --locked -- --test-threads=1
cargo test --workspace --locked
cargo fmt --all -- --check
git diff --check
```

## External Review: Preserved Invariants

- **Codex rollout persistence**: Codex persists a durable rollout/checkpoint
  before publishing results, so a crash never reports work that was not
  durably recorded. Orca preserves this invariant via the execution journal:
  `tool.completed` → `checkpoint.created` → `operation.terminal` ordering with
  atomic flush; adapters never invent terminal facts. We do not copy Codex's
  rollout wire format.
- **Grok explicit `--max-turns`**: Grok exposes an explicit turn budget on the
  command line instead of a hidden default. Orca preserves this by adding
  `--max-turns` (plus tool/cost/wall-time dimensions) and deleting
  `DEFAULT_MAX_TURNS`; interactive mode is unlimited unless bounded. We do not
  copy Grok's flag names or exit-code mapping.
- **Claude Code pre-result flush**: Claude Code flushes the final message
  before the process exits so the transcript and the exit agree. Orca preserves
  this via the journal's atomic flush and the rule that the TUI, JSONL,
  history, Goal, and Harbor projections all read the same committed
  `operation.terminal`. We do not copy Claude Code's session file layout.

## Migration and Removal

The redesign is a coordinated break: core types land first (Task 2), the
controller and journal follow (Tasks 3–4), the loop and surfaces migrate to the
typed terminal (Tasks 5–6), resume/goals/children become budget-correct
(Tasks 7–8), CLI and Harbor adopt the new protocol (Task 9), and legacy symbols
and docs are deleted (Task 10). Obsolete 128-turn fixtures are removed only
after replacement tests pass. No compatibility shim keeps `max_budget_usd` or
`BudgetExhausted` alive in production paths.
