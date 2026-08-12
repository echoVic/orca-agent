# ACP Terminal Cleanup Stall — Phase Instrumentation And Fix

## Status

Proposed for `codex/acp-terminal-stall`, based on `main` at `45db6e58c`.
Follows the reclassification in the parallel-test-isolation spec (three
failed fix hypotheses; boundary defect in the ACP terminal lifecycle under
load).

## Problem And Evidence

`acp::supervisor::tests::concurrent_terminals_from_one_tool_keep_cleanup_identity_exact`
stalls ~5-10% of default-parallelism lib runs (CI-mitigated by the nextest
`threads-required = 2` override). Two distinct stall phases are captured:

1. Round 6 (full-thread-stack capture): the test hangs in its
   `terminal/kill|release` read loop after both terminal/create
   round-trips completed; every worker and the host are parked. No
   cleanup frame is ever delivered.
2. Round 7: all three cleanup frames are delivered and answered, but the
   executor's `close()` settlement (actor-side kill-call completion) takes
   >60s; the test's `outcome_rx.recv_timeout` fires first and the worker's
   outcome send then fails (SendError).

Refuted hypotheses: busy worker past 60s (stacks show all parked); dispatch
`Full` drop (zero dispatch errors across 10 instrumented runs including a
failing one; the fail-fast dispatch contract is deliberate).

## Classification

Boundary defect: the ACP terminal cleanup lifecycle has at least two
load-coupled stall legs (frame delivery; actor-side settlement), neither
yet attributed. Production impact is plausible (a TUI/server client can
wait unboundedly for a terminal notification), but unproven — the captured
stalls occur under 12-16 way local contention.

## Scope

Instrument the full ACP terminal cleanup lifecycle with per-phase
timestamps, test-build only:

- actor dispatch (`DispatchTerminalCleanup` arm),
- client frame delivery (the `dispatch_terminal_cleanups` task writes to
  the connection),
- client response arrival (`handle_terminal_cleanup_response`),
- actor settlement start/finish (`settle_surface_acp_terminal_cleanup`),
- worker `close()` return,
- executor outcome send.

On the test-side 60s deadline, print the phase timeline for the stalled
operation and take the existing macOS `sample` thread-stack dump. All
instrumentation is `#[cfg(test)]`-adjacent (test-support module), never
shipped in production paths; a single env guard
(`ORCA_ACP_STALL_TRACE=1`) keeps it silent otherwise.

## Non-Goals

- No fix in this slice: the instrumentation's job is to attribute the slow
  leg. The fix follows with its own spec once the timeline is captured.
- No production code changes; no protocol/persistence/CLI/TUI changes.

## Ownership And Semantics

The runtime owns the capability call lifecycle; the instrumentation
borrows test-scoped observability only. Normal/cancel/reject/timeout
semantics are unchanged. The trace is bounded (fixed-size ring of
timestamped events per operation, flushed only on the deadline).

## Acceptance

1. One captured stall (capture loop under load, the round-6/7 procedure)
   produces a phase timeline that attributes the >60s gap to exactly one
   leg, with the sample dump confirming the corresponding thread state.
2. The instrumentation compiles into the test build only and is silent
   unless `ORCA_ACP_STALL_TRACE=1`.
3. `cargo test -p orca-runtime --lib --locked` green; fmt and diff-check
   clean.

## Verification Commands

```bash
cargo test -p orca-runtime --lib --locked
cargo fmt --all -- --check
git diff --check
# capture loop: repeated default-parallelism lib runs until the stall
# reproduces with ORCA_ACP_STALL_TRACE=1
```

## Round 15 Capture Status (attributed leg)

Full-chain trace (fixed instrumentation) attributes the stall: the failing
test's kill A and release frames are delivered and answered promptly
(495-711ms, each settled in ~36ms), then `worker_close_return terminal-a`
fires and — critically — the executor's `outcome_send` fires at the SAME
711ms with NO third dispatch and NO `worker_close_return terminal-b`.
The executor's scope join therefore did not wait for close B's settlement:
close B returned through the only unrecorded path — the
`cleanup_terminal` early `BrokenPipe` return when the thread command
channel is closed. A probe was added to that path
(`worker_close_broken_pipe`) to confirm on the next capture whether the
actor channel was really closed (actor death/shutdown) at that moment.

## Round 13 Capture Status

The instrumentation landed and is silent without the env guard. Four
default-parallelism runs with `ORCA_ACP_STALL_TRACE=1` did NOT reproduce
the acp stall (previous rate ~1/8-1/10), but two NEW runtime_host flakes
surfaced (recorded in the parallel-test-isolation spec): 
`host_shutdown_bounds_retained_capability_transition_without_resolving_waiter`
and `host_shutdown_preprepare_failure_cancels_and_joins_generation_before_returning_error`
both panicked with `reserve ... RuntimeUnavailable` — the reserve raced the
host shutdown and the actor dropped the reply (a shutdown-ordering race
class, not the acp terminal stall). Both are absent from the nextest
`threads-required = 2` override list. The capture loop continues; the acp
stall remains the target.

## Migration And Rollback

The instrumentation is one revertible commit; removing it restores the
current diagnostics-free tests. No persisted state.
