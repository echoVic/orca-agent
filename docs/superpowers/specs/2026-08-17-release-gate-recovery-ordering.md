# Release Gate Recovery Ordering

Status: Implemented

## Problem And Evidence

The v0.3.21 main-branch Windows gate exposed three independent release blockers:

- `goal_controller_trace_equivalence` observed generation cancellation before the
  legacy Goal Store had durably recorded `Paused`. The actor dispatched the
  blocking pause worker and cancelled the generation immediately instead of
  waiting for the worker settlement.
- `cold_owner_takeover_preserves_durable_workflow_success_before_projection`
  failed locally and on Windows for two reasons: the fixture rewrote
  `state.json` while the held child process could still overwrite it, and the
  attached persistent task registry kept the task `Running` while recovery only
  reconciled terminal workflow state into tasks already marked interrupted.
- the native ARM64 full suite reached 2,195 of 2,664 tests before the workflow's
  45-minute step timeout interrupted it.

This is a lifecycle and release-gate defect. It does not require a public API,
protocol, npm layout, or persistence-format change.

## Required Behavior

- A legacy Goal pause must durably settle the Goal Store worker before the
  active generation can observe cancellation.
- If the pause worker fails, the pause caller receives that error and the
  generation is not cancelled by the failed request.
- The pause reply still waits for generation join and terminal settlement after
  cancellation.
- The cold-owner fixture must terminate and reap the writer process before it
  injects the durable completed workflow state.
- A valid terminal workflow state is authoritative during cold takeover even
  when the attached persistent task record is still active. Recovery must
  project that terminal result into the task and operation instead of replacing
  it with a generic runtime-restart failure.
- Goal surface outbox acknowledgement remains asynchronous, so tests must wait
  for bounded eventual cleanup instead of requiring same-instant cleanup.
- Native ARM64 keeps the complete workspace test gate, with enough wall-clock
  budget to finish the measured suite.

## Ownership And Failure Semantics

`ThreadActor` remains the sole owner of generation cancellation. The blocking
Goal worker owns the durable pause write; its completion message transfers the
right to cancel back to the actor. Failed workers do not transfer that right.
The existing bounded recovery path remains responsible for an unacknowledged
Goal surface outbox record after a crash.

## Acceptance

```text
cargo test -p orca-runtime --test runtime_host goal_controller_trace_equivalence --locked -- --exact --test-threads=1
cargo test -p orca-runtime --test runtime_surface_domain goal_pause_commits_goal_state_and_operation_cancellation_before_terminal_wake --locked -- --exact --test-threads=1
cargo test -p orca-runtime --test runtime_surface_host cold_owner_takeover_preserves_durable_workflow_success_before_projection --locked -- --exact --test-threads=1
cargo test --workspace --all-targets --locked -- --test-threads=1
node scripts/test-validate-runtime-surface-contract.mjs
node scripts/validate-runtime-surface-contract.mjs
cargo fmt --all -- --check
git diff --check
```

The pushed main commit must then pass both native Windows jobs before the
v0.3.21 tag is created.
