# Headless Trajectory Truth Spec

## Problem and Evidence

The headless agent loop uses `DEFAULT_MAX_TURNS = 128` in
`crates/orca-runtime/src/agent_loop.rs`, and `RuntimeTaskActor` rejects the next
turn before admission with `RunStatus::BudgetExhausted` and
`TurnEndReason::MaxInnerTurns`. Existing `tests/agent_loop_contract.rs` proves
ordinary multi-turn success, but no headless behavior test drives the loop to
the boundary while comparing streamed JSONL and the saved transcript.

## User and Architecture Value

Terminal-bench and other headless consumers need trajectory data that tells the
truth about work performed. A run that reaches the inner-turn safety boundary
must expose a budget terminal, settle exactly once, and persist only admitted
and completed tool calls. This gives evaluators a reproducible stopping reason
without changing TUI behavior or inventing a second trajectory writer.

## Scope

In scope:

- Add a deterministic mock-provider fixture that requests one real read tool on
  every admitted model turn until the runtime's default limit is reached.
- Add a headless JSONL contract that runs the fixture with `--save-history` and
  asserts 128 started turns, 128 completed tool calls, one
  `session.completed(status=budget_exhausted)`, exit code 4, and a saved
  transcript with the same 128 tool terminal messages.
- Document the headless terminal/trajectory boundary in the roadmap.

Out of scope:

- Changing `DEFAULT_MAX_TURNS`, CLI flags, JSONL event names, exit-code mapping,
  persistence schema, TUI rendering, provider retry policy, or auto-memory.
- Adding a new trajectory format or a second persistence fact source.

## Lifecycle and Failure Semantics

- Turns 1 through 128 are admitted normally; each fixture tool request executes
  and records one terminal tool result.
- Turn 129 is rejected before provider admission as `BudgetExhausted` with
  `MaxInnerTurns`; no provider call or tool side effect is created for it.
- The controller emits one terminal `session.completed` event with
  `status=budget_exhausted` and returns `RunStatus::BudgetExhausted.exit_code()`
  (4).
- If persistence or event publication fails, the existing controller error
  path remains authoritative; this slice adds no retry or recovery owner.

## Ownership and Boundaries

`RuntimeTaskActor` owns the inner-turn admission boundary. `ThreadTurnExecutor`
owns provider/tool execution and completion settlement. `EventSink` owns the
streamed JSONL projection, while `SessionWriter` owns the saved transcript.
The test compares these existing projections and does not introduce a separate
trajectory recorder.

## Compatibility

No CLI argument, JSONL payload, exit code, persistence record, or provider API
changes. The mock fixture is test-only behavior selected by its prompt string.

## Acceptance Criteria

1. The focused headless test fails before the fixture/contract implementation
   because the new repeated-turn prompt is not supported.
2. The focused test passes with exactly 128 `turn.started`, 128
   `tool.call.completed`, one `session.completed` carrying
   `budget_exhausted`, process exit code 4, and no event after the session
   terminal.
3. The saved JSONL transcript contains exactly 128 tool conversation messages,
   each with the existing flattened terminal metadata
   (`status=completed`, `kind=success`, and `exit_code=0`), and no record for
   an unadmitted 129th call.
4. Existing agent-loop/provider contracts, formatting, diff checks, and the
   required serial runtime lifecycle gate pass.

## Final Evidence

- RED confirmed before the fixture branch: the new prompt returned process exit
  code `0` instead of the required budget terminal.
- The deterministic fixture now requests `mock-repeat-read-1` through
  `mock-repeat-read-128`; the runtime rejects the next turn before provider
  admission.
- The saved transcript uses the established flattened terminal fields rather
  than a nested object; the contract asserts `status=completed`,
  `kind=success`, and `exit_code=0` for all 128 tool records.

## Verification Commands

```bash
cargo test --test agent_loop_contract headless_max_inner_turns_preserve_trajectory_truth -- --exact --nocapture
cargo test --test agent_loop_contract -- --test-threads=1
cargo test --test runtime_lifecycle_contract -- --test-threads=1
cargo fmt --all -- --check
git diff --check
```

## Migration and Removal

This is a test-and-contract slice. The deterministic provider branch is retained
as a named fixture for future headless lifecycle regressions; no production
compatibility shim or duplicate runtime loop is added. Reverting the semantic
commit removes only the fixture, test, and roadmap evidence.
