# Task 4 Detached Authority Report

## Scope

This change closes the detached relay and asynchronous subagent authority
gaps. No `crates/orca-tui` files were changed by this task.

## RED regressions

- A JSONL ledger reopened after restart returned no source digest for a
  `ChildThreadBound` event. The durable source-digest index only recognized
  `Started`, `Progress`, and `Completed` patches.
- A child activity with a missing or out-of-order `parent_task_id` could be
  projected as a root task because invalid registry data was filtered to
  `None`.
- Async launch created the worker before the parent activity stream had a
  durable `Started` event, and the worker allocated a separate turn identity.
- Relay corruption could be reported repeatedly on every actor tick instead of
  becoming a sticky typed health/runtime-fault quarantine.

## Implementation

- The actor now commits detached `Started` through its actor-owned ingress
  before `mark_worker_spawned` or process spawn. The worker receives the same
  `TurnId`, carries `--activity-start-precommitted`, and starts relay source
  sequence at 2. Task-control continuation launches use the same ingress.
- Spawn, adoption, and worker ownership failures commit the continuation
  terminal first, then exactly one failed surface terminal with the matching
  identity. If `Started` itself is rejected, no sequence-two terminal is
  attempted; continuation/task state fails closed without fabricating a gap.
- `ChildThreadBound` now participates in the durable source-digest index,
  preserving retry/conflict checks across actor restart.
- Parent task resolution rejects missing registry entries, missing parent IDs,
  invalid IDs, and parents absent from the authoritative surface task tree.
- Corrupt relays are quarantined once with typed health and runtime-fault
  events; later ticks are idempotent no-ops.
- Worker CLI/request parsing validates and propagates the actor-assigned child
  turn identity and precommit flag.

## Verification

Passed:

```text
cargo fmt --all -- --check
cargo check --workspace --locked
cargo test -p orca-runtime --lib subagent_async_worker::tests --no-fail-fast --locked
cargo test -p orca-runtime --lib child_agent_types::tests::precommitted_activity_emitter_starts_at_sequence_two --locked
cargo test -p orca-runtime --lib task_transcript_query_tests::subagent_parent_resolution --locked
cargo test -p orca-runtime --test runtime_surface_commit jsonl_ledger_indexes_child_thread_bound_source_digest_after_restart --locked
cargo test -p orca-runtime --test subagent_observability_contract --locked
git diff --check
```

The focused results were 4, 1, 2, 1, and 13 tests passed respectively.

The complete runtime suite was not claimed green: unrelated existing network
policy tests are environment-sensitive, and one cancellation-drain test was
observed flaky in the broader all-target run. Those failures are outside this
task's detached relay changes.

## Remaining risk

Worker failures before it can construct its relay emitter (for example, a
missing detached binding during adoption) cannot independently publish a
surface terminal. Parent actor spawn/adoption failure paths are terminalized;
future work could add a durable actor watchdog for post-spawn worker bootstrap
failures.
