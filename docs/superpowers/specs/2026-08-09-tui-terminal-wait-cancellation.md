# TUI Terminal Wait Cancellation Specification

## Problem and root cause

`crates/orca-tui/src/surface_client.rs::drain_operation_with_boundary` starts a
background thread for `wait_operation_terminal`. The runtime registers only a
reply sender, so the wait has no caller-owned cancellation state. When the TUI
hands an operation to the background surface or encounters a sealed
subscription, it can return before the wait finishes; on the failure path it
waits only five seconds and then drops the `JoinHandle`. This permits a
detached waiter and makes shutdown latency depend on an unbounded runtime
wait.

This is an architecture/lifecycle defect, not a timeout tuning problem. The
runtime surface already has a `WaitCancelled` result and an unused
`WaitOperationTerminalRequest::caller_cancel` type, but the live command path
does not carry or observe it.

## User value

TUI background handoff, shutdown, projection failure, and subscription close
must return without leaving a waiter thread behind. A terminal wait canceled by
its caller must be visible as a typed `WaitCancelled` result, and every waiter
created by the TUI must be joined before the drain function returns.

## Scope and non-goals

In scope:

- make `OptionalProcessLocalCancel` a one-shot process-local cancellation
  signal;
- carry it through the runtime surface terminal-wait command;
- store it with each runtime-owned terminal waiter and settle canceled waiters
  as `WaitOperationTerminalResult::WaitCancelled`;
- have the runtime actor poll cancellation while otherwise idle or running;
- make the TUI waiter use this signal and join unconditionally after requesting
  cancellation.

Out of scope:

- changing operation terminal state, persistence, or public JSONL protocol;
- changing normal terminal-wins ordering;
- introducing a second operation lifecycle or a client-owned terminal cache;
- solving unrelated renderer-owned TUI orchestration.

## Semantics

- A terminal already cached when the wait command is handled wins over a
  concurrent cancellation and returns `Terminal`.
- A registered waiter whose caller signal is canceled before terminal caching
  is removed by the runtime actor and returns `WaitCancelled`.
- Runtime shutdown still returns `RuntimeUnavailable` through the existing
  shutdown drain path.
- The token is monotonic: cancellation can only move from false to true and
  cannot be reset or reused for a later wait.
- The TUI cancels its waiter before every early return and joins it without a
  timeout. The join is bounded by the runtime's cancellation polling interval,
  not by an unbounded terminal wait.

## Ownership and compatibility

The runtime owns the registered waiter record and the only state transition
that settles it. The caller owns a clone of the one-shot cancellation signal.
The surface command remains process-local and does not alter persisted records,
CLI arguments, TUI flow, or server/JSONL wire shapes. Existing callers keep the
current `wait_operation_terminal` convenience method, which creates a fresh
uncanceled signal; cancellable callers use the explicit method.

## Acceptance criteria

1. A runtime surface terminal waiter canceled before terminal commit returns
   `WaitOperationTerminalResult::WaitCancelled` and is removed from the
   controller.
2. A terminal committed before cancellation still returns `Terminal`.
3. The TUI drain path cancels and joins its waiter on background handoff,
   projection failure, subscription sealing, and terminal recovery failure;
   no timeout path drops a live `JoinHandle`.
4. Existing runtime surface lifecycle, TUI lifecycle, and full package tests
   remain green.

## Verification

```bash
cargo test -p orca-runtime runtime_actor::commit --lib -- --test-threads=1
cargo test -p orca-runtime runtime_surface --lib -- --test-threads=1
cargo test -p orca-tui surface_client --lib -- --test-threads=1
cargo test -p orca-runtime --lib -- --test-threads=1
cargo test -p orca-tui --lib -- --test-threads=1
cargo fmt --all -- --check
git diff --check
```

## Migration and rollback

The old convenience wait method remains source-compatible while the TUI moves
to the explicit cancellation method. The old timeout/drop branch is deleted
in the same commit as the runtime command plumbing. Rollback is a single
commit revert; no persisted migration is required.
