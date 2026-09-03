# Task 3 Runtime Convergence Report

## Scope

Task 3 convergence was implemented in the runtime child execution path. Existing dirty changes in the workspace were preserved. The implementation keeps independent hosted child runtime threads, routes all controller-backed synchronous requests through the observable activity path, and keeps headless/injected execution on a real non-discarding activity adapter.

## RED Evidence

Production-path tests were added before the implementation correction and run with:

```text
cargo test -p orca-runtime subagent_execution::tests --lib --locked -- --test-threads=1
```

The initial run failed for the intended defects:

- `stopping_one_hosted_sibling_does_not_cancel_the_other_or_the_parent`: per-child stop cancelled the shared parent token.
- `parent_cancellation_propagates_to_every_hosted_child`: hosted children did not all receive cancellation.
- `threaded_sync_registry_stop_interrupts_the_hosted_child`: registry stop returned a failed result instead of `ToolStatus::Cancelled`.
- `hosted_sync_schema_request_uses_the_observable_runtime_route`: schema request entered the wrong compatibility executor.
- `hosted_sync_resume_rejects_before_child_launch`: resume was admitted/launched instead of failing before child launch.
- `hosted_surface_terminal_observes_committed_continuation_terminal`: surface terminal was published before continuation state was committed.

## Implementation

- Allocated a private `CancelToken` for each controller-backed admitted child. A parent-cancellation bridge fans out to each private token; registry stop bridges only to that child token. Watcher spawn failures are returned as typed pre-start failures, and normal worker completion explicitly terminates the watcher.
- Preserved `ToolResult::cancelled` for hosted cancellation rather than converting it to `failed_after_start`.
- Removed `LegacySubagentActivitySink`. Production synchronous execution without actor-owned ingress fails closed; unit-only headless tests use an explicit non-discarding collector.
- Removed the semantic `should_use_threaded_agent_worker` predicate. Controller-backed requests take the hosted route; unsupported hosted `resume_from` and worktree isolation fail closed before launch with precise typed messages. Schema output is validated before the result status is finalized.
- Reused the canonical registry task identity for hosted surface and permission binding. Hosted child thread IDs remain typed `ChildThreadBound` metadata.
- Finalized continuation/schema/registry state before publishing the continuation-backed surface terminal, with exactly one terminal activity event.
- Added regression coverage for sibling cancellation isolation, parent cancellation fan-out, typed cancelled results, schema/resume admission, missing ingress, identity binding, and terminal ordering.

## GREEN Evidence

```text
cargo test -p orca-runtime subagent_execution::tests --lib --locked -- --test-threads=1
29 passed; 0 failed

cargo test -p orca-runtime runtime_subagent_call::tests --lib --locked -- --test-threads=1
4 passed; 0 failed

cargo test -p orca-runtime agent_controller::tests --lib --locked -- --test-threads=1
1 passed; 0 failed

cargo check -p orca-runtime --all-targets --locked
Finished; no warnings

cargo fmt --all -- --check
clean

git diff --check
clean
```

Obsolete-path search:

```text
rg -n 'LegacySubagentActivitySink|should_use_threaded_agent_worker|EventSink::new\(io::sink\(\)' \
  crates/orca-runtime/src/runtime_subagent_call.rs \
  crates/orca-runtime/src/subagent_async_worker.rs \
  crates/orca-runtime/src/agent_controller.rs \
  crates/orca-runtime/src/subagent_execution.rs
```

No matches.

## Staged Files

The focused commit stages only:

- `crates/orca-runtime/src/agent_controller.rs`
- `crates/orca-runtime/src/runtime_subagent_call.rs`
- `crates/orca-runtime/src/subagent_execution.rs`
- `.superpowers/sdd/2026-08-30-subagent-observability-user-trust-recovery/task-3-runtime-convergence-report.md`

Other pre-existing dirty files, including TUI and runtime-surface files outside this task's direct implementation boundary, remain unstaged.

## Concern

Hosted controller-backed `resume_from` and worktree isolation are intentionally rejected before child launch because the hosted controller does not yet provide those continuation/worktree capabilities. This is fail-closed and typed per the Task 3 contract; the existing continuation path remains available for non-hosted execution.

## Fix Round 4

### RED

The reviewer-driven RED additions reproduced four issues: hosted invalid schema published a green surface terminal; terminal publication failure returned `Completed`; a missing-ingress injected sync call launched a child; and the hosted ordering fixture did not actually exercise the continuation path. The canonical task/thread binding regression already passed and was retained.

### Changes

- `AgentController::settle_operation` now returns authoritative status/data without publishing a terminal. The synchronous caller validates schema, settles the task mirror, publishes the one surface terminal, and converts ambiguous terminal publication failure to an indeterminate tool result.
- Removed `RegistrySubagentActivityIngress` and all task-registry presentation fallback. Production synchronous execution without actor-owned ingress fails closed; unit-only headless tests use an explicit non-discarding collector.
- Recomputed returned runtime task status after schema and terminal publication outcomes, so invalid schema yields both task and surface `Failed`.
- Invalid delegated model parsing publishes a failed terminal after `Started` before returning the launch error.
- Parent-cancellation watcher cancellation is triggered on worker spawn failure; successful workers terminate their watcher on return.
- Corrected the continuation-order fixture to use the continuation-backed child path and added live child thread/transcript identity assertions.

### GREEN

```text
cargo test -p orca-runtime subagent_execution::tests --lib --locked -- --test-threads=1
31 passed; 0 failed

cargo test -p orca-runtime runtime_subagent_call::tests --lib --locked -- --test-threads=1
4 passed; 0 failed

cargo test -p orca-runtime agent_controller::tests --lib --locked -- --test-threads=1
1 passed; 0 failed

cargo check -p orca-runtime --all-targets --locked
Finished; no warnings

cargo fmt --all -- --check
clean

git diff --check
clean
```

Hosted invalid-schema evidence: the focused test asserts `ToolStatus::Failed`, the returned lifecycle task is `TaskStatus::Failed`, and the sole surface terminal is `SurfaceSubagentTerminalStatus::Failed`. Terminal publication failure evidence: the focused test asserts `ToolStatus::Indeterminate` with an external-state warning and zero green terminal events. Launch failure evidence: `threaded_sync_launch_failure_finishes_the_canonical_registry_task` asserts failed task settlement and failed surface terminal.
