# TUI Terminal Wait Cancellation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make TUI terminal waits runtime-owned, cancelable, and unconditionally joined without changing external protocols.

**Architecture:** Carry a one-shot `OptionalProcessLocalCancel` through the runtime surface wait command. The runtime commit controller stores the signal with each waiter and retires canceled waiters as typed `WaitCancelled` results; the actor polls this narrow queue while its existing select loop is otherwise idle or running. The TUI creates one signal per waiter, cancels it on every early return, and always joins.

**Tech Stack:** Rust, `std::sync::atomic`, Tokio actor loop, runtime surface command types, TUI surface client.

---

### Task 1: Add RED cancellation contracts

**Files:**
- Modify: `crates/orca-runtime/src/runtime_surface/identity.rs`
- Modify: `crates/orca-runtime/src/runtime_actor/commit.rs`
- Modify: `crates/orca-runtime/src/runtime_host.rs`
- Test: existing module tests in the same files

- [x] **Step 1: Add a one-shot cancellation signal test.**

Add a unit test asserting a fresh signal is not canceled, `cancel()` makes it
canceled, and a second `cancel()` does not reset or change the state.

- [x] **Step 2: Add a canceled terminal waiter controller test.**

Register one waiter with a canceled signal, call the controller's cancellation
retirement method, apply the returned reply effect, and assert the receiver gets
`WaitOperationTerminalResult::WaitCancelled` and the waiter count becomes zero.

- [x] **Step 3: Run the tests and verify RED.**

Run:

```bash
cargo test -p orca-runtime runtime_surface::identity::tests::terminal_wait_cancel_signal_is_one_shot --lib -- --test-threads=1
cargo test -p orca-runtime runtime_actor::commit::tests::cancelled_terminal_waiter_is_retired --lib -- --test-threads=1
```

Expected: the tests do not compile or fail because the signal has no state and
the commit controller has no cancellation retirement behavior.

### Task 2: Implement runtime-owned cancellation

**Files:**
- Modify: `crates/orca-runtime/src/runtime_surface/identity.rs`
- Modify: `crates/orca-runtime/src/runtime_surface/commands.rs`
- Modify: `crates/orca-runtime/src/runtime_actor/commit.rs`
- Modify: `crates/orca-runtime/src/runtime_host.rs`

- [x] **Step 1: Implement the monotonic process-local signal.**

Store `Arc<AtomicBool>` in `OptionalProcessLocalCancel`, expose public
`new`, `cancel`, and `is_cancelled` methods, and use `Ordering::Release` for
the write and `Ordering::Acquire` for reads. Do not add reset or replacement
operations.

- [x] **Step 2: Add the explicit cancellable surface wait method.**

Keep `RuntimeSurfaceClientHandle::wait_operation_terminal` as a convenience
wrapper that creates `OptionalProcessLocalCancel::new()`. Add
`wait_operation_terminal_with_cancel(request_id, operation_id, caller_cancel)`
and carry `caller_cancel` in `ThreadCommand::SurfaceWaitOperationTerminal`.
Implement the corresponding dispatcher method in `ThreadSurfaceDispatcher` and
pass the signal into `wait_surface_operation`.

- [x] **Step 3: Store and retire cancellable waiters.**

Change the commit controller's private waiter record to hold both the reply
sender and `OptionalProcessLocalCancel`. Preserve existing terminal settlement
ordering. Add `has_terminal_waiters` and a method that removes only canceled
waiters, returning `RuntimeActorEffect::ReplyOperation` values with
`WaitCancelled`.

- [x] **Step 4: Poll cancellation in the actor loop.**

Before each idle/running select, record whether terminal waiters exist. Add a
25ms sleep branch guarded by that flag and apply the controller's cancellation
effects. Keep the existing terminal commit branch first in the biased select so
a committed terminal wins a same-tick cancellation race.

- [x] **Step 5: Run GREEN focused tests.**

Run:

```bash
cargo test -p orca-runtime runtime_actor::commit --lib -- --test-threads=1
cargo test -p orca-runtime runtime_surface --lib -- --test-threads=1
```

Expected: the new cancellation tests and existing runtime surface tests pass.

### Task 3: Make the TUI waiter cancel and join

**Files:**
- Modify: `crates/orca-tui/src/surface_client.rs`
- Test: existing `surface_client` lifecycle tests

- [x] **Step 1: Create one cancellation signal per waiter.**

Replace the raw `wait_operation_terminal` call with
`wait_operation_terminal_with_cancel` and move a cloned signal into the waiter
thread. Keep the signal in the drain function so early-return branches can
cancel it.

- [x] **Step 2: Centralize waiter shutdown.**

Add a local helper or guard that cancels the signal before every return path and
joins the handle unconditionally. Delete the five-second `recv_timeout` branch
and the conditional join based on `waiter_finished`. A terminal result received
before cancellation remains the normal success path.

- [x] **Step 3: Add a behavioral TUI regression test.**

Exercise a sealed/failing operation drain with a waiter that cannot terminalize
until its caller signal is canceled. Assert the drain returns promptly with its
original error and the waiter completion marker is observed before the test
returns. The test must observe behavior through the runtime surface, not inspect
source text or a detached-thread flag.

- [x] **Step 4: Run the focused TUI gate.**

```bash
cargo test -p orca-tui surface_client --lib -- --test-threads=1
```

Expected: the focused lifecycle suite passes with no timeout-based cleanup.

### Task 4: Documentation, full gates, and delivery

**Files:**
- Modify: `docs/production-roadmap.md`
- Modify: `docs/superpowers/specs/2026-08-09-tui-terminal-wait-cancellation.md`
- Modify: `docs/superpowers/plans/2026-08-09-tui-terminal-wait-cancellation.md`

- [x] **Step 1: Record the completed runtime/TUI ownership boundary and test evidence.**

Update the roadmap row for TUI event/interaction adapters to state that
terminal observation waits now use runtime-owned cancellation and unconditional
join semantics.

- [x] **Step 2: Run the full affected gates.**

```bash
cargo test -p orca-runtime --lib -- --test-threads=1
cargo test -p orca-tui --lib -- --test-threads=1
cargo fmt --all -- --check
git diff --check
```

- [x] **Step 3: Review, commit, rebase, and rerun.**

Review the diff for duplicate waiter ownership, resettable cancellation,
terminal-wins ordering, and external compatibility. Commit the slice, fetch and
rebase `origin/main`, then rerun focused and full affected gates before
fast-forwarding clean `main`.
