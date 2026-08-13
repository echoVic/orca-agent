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
- `ExecutionJournal`: ordered records (`operation.started`, `budget.usage`,
  `turn.started`, `model.response`, `tool.started`, `tool.completed`,
  `checkpoint.created`, `operation.terminal`) with atomic flush;
  JSONL/transcript projections feed only from committed records.
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

## Review Round 3 Addendum: Suspension, Stateless Stops, and Lease Settlement

Classification: boundary defects found while re-reviewing the round-2 fixes.
Each closes a case where a terminal could misreport the durable state.

1. **Suspended exchange resumes accounting from its snapshot.** The
   suspended-provider completion path reopens the journal with a fresh
   controller, so pre-suspension turns, tool calls, and cost were forgotten and
   the exchange total could be undercounted. `SuspendedOperationHandle` now
   carries the controller `usage` at suspension and `BudgetController::
   restore_usage` restores it before the provider cost/wall-time delta is
   recorded. Verified by
   `runtime_host::tests::suspended_exchange_resumes_accounting_from_snapshot_and_stops_over_budget`.

2. **An over-budget suspended exchange commits a non-resumable Stopped
   terminal.** The background completion path has no durable conversation
   boundary, so a success terminal for an over-budget exchange was a lie, and a
   resumable stop would have claimed a boundary that does not exist.
   `settle_suspended_operation` derives the terminal from the controller AFTER
   accounting: over-budget commits `commit_non_resumable_budget_stop`
   (checkpoint recorded with no message id, terminal `resumable: false`) and
   failures now propagate as `io::Result` instead of being silently dropped.
   `ApprovalRequired` without a budget stop still stays parked; approval plus
   an exhausted budget surfaces the stop instead of parking forever. Verified
   by the same runtime_host test.

3. **Stateless budget stops are never resumable.** With no session writer there
   is no durable message boundary; `commit_budget_stop_with_boundary` now
   commits the non-resumable path for stateless operations (the terminal still
   carries its journal checkpoint id). Verified by
   `operation_context::tests::stateless_budget_stop_commits_non_resumable_terminal_without_boundary`
   and `tests/agent_loop_contract.rs::headless_budget_stop_skips_verifier_and_keeps_stopped_terminal`
   (resumable false, checkpoint id present).

4. **Session checkpoint reason is dimension-specific.** Turn, tool-call, and
   wall-time stops each write their own reason
   (`turn_budget_exhausted`, `tool_call_budget_exhausted`,
   `wall_time_budget_exhausted`) instead of masquerading as cost exhaustion;
   the cost string stays byte-stable for existing history consumers. Verified
   by `tests/history_contract.rs::turn_budget_exhausted_session_writes_distinct_checkpoint_reason`.

5. **Batch children partition the configured spec evenly.** The first child
   lease can no longer monopolize the parent's finite remainder:
   `divide_spec_across_children` shares each bounded dimension (floored at one;
   unlimited stays unlimited) before leasing, and the controller still
   intersects with actual remaining capacity, so children can never double
   spend. Verified by
   `tool_turn::tests::batch_child_spec_partitions_finite_dimensions_and_keeps_unlimited`.

6. **Lease refusal settles in journal order.** When the parent refuses a batch
   child lease, settlement order is: granted leases, then every admitted batch
   tool as cancelled (`tool.completed` durable), then the conversation, then
   the session boundary, then the journal checkpoint + terminal — a checkpoint
   can never precede open tool settlement. A refused single-subagent lease is a
   typed budget stop, never a silent fallback to an unbounded child.

7. **Child receipts survive persistence failures.**
   `run_subagent_batch_tool_turn` returns the child usage receipts alongside
   its `io::Result`, so the parent still charges exactly what each completed
   child consumed when the event stream fails after terminals are recorded.
   Verified by
   `subagent_execution::tests::batch_persistence_failure_preserves_completed_child_receipts`.

8. **Tool settlement carries the committed outcome.** Conversation tool
   settlement reads both the committed status label and the committed terminal
   error; the journal never disagrees with the conversation about a tool's
   outcome and never hardcodes success.

Compatibility: no CLI, JSONL envelope, or persisted-format shape changes; the
round only corrects terminal facts (reason strings add new dimension-specific
values, resumability becomes truthful). Normal, cancellation, rejection,
timeout, retry, disconnect, and restart semantics are unchanged from the
redesign contract.

Acceptance: the five named tests pass plus the full focused gate below:

```bash
cargo test --test execution_budget_contract --locked
cargo test --test agent_loop_contract --locked -- --test-threads=1
cargo test --test history_contract --locked -- --test-threads=1
cargo test --test exec_jsonl --locked -- --test-threads=1
cargo test -p orca-runtime --test budget_lease_contract --locked -- --test-threads=1
cargo test -p orca-runtime --test budget_resume_contract --locked -- --test-threads=1
cargo test -p orca-runtime --test operation_terminal_contract --locked
cargo test -p orca-runtime --test execution_journal --locked -- --test-threads=1
cargo test -p orca-runtime --lib -- operation_context::tests::stateless_budget_stop \
  runtime_host::tests::suspended_exchange tool_turn::tests::batch_child_spec \
  subagent_execution::tests::batch_persistence
```

## Review Round 4 Addendum: Durable Accounting and Fair Remaining Capacity

Round 3 still treated the suspended controller snapshot as recovery authority
and partitioned batch children from the original config. That was insufficient
for process restart, approval continuation, and settlement retry.

1. Journal schema v2 stores the immutable `BudgetSpec` in
   `operation.started` and appends cumulative `budget.usage` facts at every
   admission/accounting boundary. Reopen restores the newest committed usage,
   reconstructs elapsed wall time from the original absolute start timestamp,
   and rejects a requested spec that differs from the durable operation spec.
2. Provider cost settlement uses the stable response item id as its accounting
   id. Reopening and retrying the same response observes the committed id and
   does not charge it twice. A duplicate retry still persists a fresh wall-time
   fact. Foreground provider accounting derives the response delta from the
   `CostTracker` before and after that call, never by subtracting the durable
   controller's potentially different cumulative baseline. Tool admission uses
   the operation-local monotonic admission count instead of provider tool-call
   ids, because providers may legally reuse an id for a schema-failed retry.
3. Batch child leases are allocated together from the controller's current
   remaining and already-reserved capacity. Additive dimensions are split
   fairly; the entire allocation is refused if any finite dimension has less
   than one unit per child. Wall time is a shared deadline, not an additive
   receipt, so parallel child elapsed values are never summed.
4. Child leases enforce wall time after zero-cost provider responses, and
   receipts include provider cost, child turns, tool calls, and nested child
   work even when later conversation/event persistence fails.
5. Typed provider outcomes persist the core `OperationTerminal`; live,
   recovery, Goal, and legacy task projections classify a durable stopped
   terminal as budget exhaustion instead of rebuilding the result from the
   provider status.
6. Hosted resumable generations defer a cancellation terminal to the host.
   The host commits `Cancelled` only after deciding that no queued resume will
   replace the generation; a successor can therefore append to the same
   logical turn journal without violating the terminal-is-final invariant.

Compatibility is intentionally broken at the operation-journal boundary.
Schema v1 and mixed-version records are explicitly rejected. Saved sessions
remain valid resume boundaries, but a v1 in-flight operation journal is not
migrated or continued.
