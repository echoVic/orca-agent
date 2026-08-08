# Headless Terminal Projection Contract Specification

## Problem and Evidence

`tests/agent_loop_contract.rs::headless_max_inner_turns_preserve_trajectory_truth`
currently counts streamed `tool.call.completed` events and persisted tool
conversation records separately. It verifies both collections have 128 entries
and use the expected fixture ids, but it does not compare the ordered terminal
metadata across the two projections. A stream/transcript mismatch could
therefore pass while the test claims trajectory truth.

## User and Architecture Value

Headless evaluators and TUI/server consumers must be able to trust that the
visible terminal for each admitted tool call is the same terminal persisted for
resume and audit. The existing JSONL event sink and SessionWriter remain the
only projection owners; this slice strengthens their contract without adding a
third recorder or changing runtime behavior.

## Scope and Non-Goals

In scope:

- compare the ordered streamed and persisted terminal projections for the
  repeated-read boundary fixture;
- assert the shared tool-call id, status, kind, and exit-code fields for every
  admitted call;
- document the stronger evidence and run the existing headless/runtime gates.

Out of scope:

- changing the runtime loop, event schema, persistence schema, provider fixture,
  max-turn policy, or TUI rendering;
- adding a second projection source or broadening the expensive headless run;
- changing unrelated terminal contract tests.

## Semantics and Ownership

- The event sink owns streamed `tool.call.completed` JSONL records.
- The SessionWriter owns persisted `conversation.message` tool terminals.
- The contract extracts only their existing terminal fields and compares them in
  admission order; neither projection is rewritten by the test.
- A missing, extra, reordered, or metadata-mismatched terminal fails the test.
- The runtime's existing max-turn rejection and terminal settlement remain
  authoritative.

## Compatibility

No CLI, TUI, server/JSONL, provider, persistence, or public Rust API changes.
The change is test and documentation only.

## Acceptance Criteria

1. The focused contract asserts equal ordered `(id, status, kind, exit_code)`
   tuples from streamed completion events and persisted tool messages.
2. The existing 128-turn boundary assertions remain intact, including exit code
   4, one final `budget_exhausted` session terminal, and no unadmitted 129th id.
3. The focused agent-loop contract, serial runtime lifecycle contract,
   formatting, and diff checks pass.

## Verification

```bash
cargo test --test agent_loop_contract headless_max_inner_turns_preserve_trajectory_truth -- --exact --nocapture
cargo test --test agent_loop_contract -- --test-threads=1
cargo test --test runtime_lifecycle_contract -- --test-threads=1
cargo fmt --all -- --check
git diff --check
```

## Final Evidence

- The focused boundary contract passed with ordered event/transcript tuple
  equality in 48.62 seconds.
- The serial `agent_loop_contract` suite passed 4/4 tests in 51.93 seconds.
- The serial `runtime_lifecycle_contract` suite passed 54/54 tests in 2.81
  seconds.
- `cargo fmt --all -- --check` and `git diff --check` passed.

## Migration and Rollback

This slice changes no runtime or persisted data. Reverting its single semantic
commit removes the stronger contract and its evidence without migration.
