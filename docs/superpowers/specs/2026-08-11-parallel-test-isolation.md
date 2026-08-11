# Parallel Test Isolation And Goal Store Concurrency

## Status

Proposed for `codex/parallel-test-isolation`, based on `origin/main` at
`2dfb21a4f85164e8ec687afbb66b4cd963bdc805` (v0.3.14 + review regressions fix).

## Problem And Evidence

Two review follow-ups remain after `2dfb21a4f`:

1. `cargo test -p orca-runtime --test runtime_host --locked` (default
   parallelism) still fails intermittently: `capability_controller_trace_equivalence`
   panics with `ThreadStartFailed { message: "failed to inspect Goal store for
   surface recovery: goal database error: database is locked" }`. Observed once
   in 43 runs (~2.3%).
2. `cargo test -p orca-runtime --lib --locked` (default parallelism) fails
   46-57 tests. The same suite passes under nextest ci profile
   (test-threads=2, retries=2). A v0.3.13 baseline checkout shows 41 failures
   under the same command, so this is a pre-existing isolation defect, not a
   regression from the review fix.

### Root Cause (evidence-based)

- `GoalStore::open_internal` unconditionally runs `initialize_schema()`, which
  opens a new SQLite connection and executes an `Immediate` DDL transaction.
  `GoalStore::connection()` opens a fresh `Connection` per call and re-applies
  `journal_mode=WAL` and `busy_timeout(5s)` every time. When many tests
  concurrently open the same process-wide goal database, the DDL transaction
  contends and some opens fail with `database is locked` / `disk I/O error`, or
  read before `goal_meta` exists (`no such table`).
- The runtime_host integration binary now uses one process-wide `ORCA_HOME`
  (fix from `2dfb21a4f`), so all tests share one `goals.sqlite3`. Before that
  fix, each test used its own temporary home and never contended on the goal
  store; the fix traded directory-deletion races for goal-store contention.
- The lib test binary (`crates/orca-runtime/src/**`) has no process-wide
  `ORCA_HOME`: tests that touch the goal store or recorded sessions share the
  real `~/.orca` (or whatever `ORCA_HOME` is set to) and its `goals.sqlite3`.
  46-57 failures under `cargo test` default parallelism show up as:
  - `goal database error: disk I/O error` / `database is locked` /
    `no such table: goal_meta` (goal-store contention);
  - `no saved session matches '<id>'` (recorded sessions from one test are
    removed or never visible to another concurrent test using the same home);
  - `prepared surface owner identity changed during materialization` (surface
    bootstrap raced with another test's session state).

This is a boundary defect in test isolation plus a latent concurrency defect in
`GoalStore` open/initialize.

## User Value

- CI/developer gate reliability: `cargo test` default parallelism is the
  canonical local verification command. A suite that fails 1-2% of runs
  undermines every "tests pass" claim and masks real regressions.
- `GoalStore` concurrent-open hardening benefits real multi-process usage
  (server + TUI + CLI can open the same goal database) and removes a
  hidden dependency on the 5-second busy timeout being long enough.
- No behavior change for TUI users; this slice is test/infrastructure quality.

## Scope

1. Make `GoalStore::open_internal` safe under concurrent opens:
   - Acquire a cross-process lock around schema initialization (the existing
     `ExclusiveFileLock` used by the task registry is the platform precedent),
     or perform initialization only when the schema is missing, inside one
     guarded critical section.
   - Keep `busy_timeout` and WAL behavior unchanged for existing opens.
2. Give the lib test binary a process-wide isolated `ORCA_HOME` equivalent to
   the integration binary's `isolated_orca_home()`:
   - Introduce a shared test-support helper (e.g. in `orca-runtime`'s
     `#[cfg(test)]` test-support module) that lazily sets `ORCA_HOME` to one
     process-lifetime temp dir via `OnceLock`.
   - Apply it at the entry points the failing lib tests actually use
     (goal store open, recorded session start), without wrapping every test
     in a mutex (that would serialize the whole suite and hide the real
     fix needed for goal-store concurrency).
3. Keep `cargo test --lib` default parallelism green across repeated runs and
   `runtime_host` integration default parallelism green across repeated runs.

## Non-Goals

- Do not wrap all tests in a global mutex; that would serialize the suite and
  mask concurrency defects rather than fix them.
- Do not change the goal database schema, persisted formats, or any external
  protocol.
- Do not convert `GoalStore` to a long-lived pooled connection in this slice;
  the open-time contention is the observed failure. Connection pooling can be a
  later slice if multi-process usage demands it.
- Do not touch TUI, server/JSONL, CLI, or release behavior.

## Ownership And Concurrency Model

- `GoalStore` owns the goal SQLite file. Schema initialization is one logical
  action and must be atomic across processes and threads that call
  `GoalStore::open`/`load_default` concurrently. The store file remains the
  single authority; a short lock around `initialize_schema` does not change
  read/write ownership of rows afterward.
- The lib test binary owns its `ORCA_HOME` via one `OnceLock<TempDir>`; the
  temp dir lives for the whole test process and is never removed mid-run.
  Tests that intentionally share state keep their existing mutex; tests that
  need isolation get it from the distinct temp dir per binary.
- No new long-lived background owner is introduced.

## Normal And Failure Semantics

| Situation | Behavior |
|-----------|----------|
| Two threads open the same goal DB simultaneously | One initializes the schema under the cross-process lock; the other waits and then proceeds. Both succeed. |
| Schema already exists | Initialization lock is taken briefly, schema check finds tables present, no DDL is repeated. |
| First open after DB file creation | Lock held across `create_dir_all` + DDL; all later opens see a complete schema. |
| Lock acquisition fails (I/O error) | Open returns the existing `GoalStoreError`; no partial schema is assumed. |
| Test process runs many tests in parallel | All goal-store opens serialize only the initialization critical section; row operations are unaffected. |
| Lib test binary starts | `ORCA_HOME` is set once to a process-lifetime temp dir before any test resolves it. |

## Compatibility And Migration

No CLI, TUI, server/JSONL, persistence, or public Rust symbol changes. The
goal store file format is untouched. `GoalStore::open` semantics for a single
opener are unchanged. Test-support helper is `#[cfg(test)]` only.

## Acceptance Criteria

1. `GoalStore` concurrent-open test: N threads opening the same new database
   path all succeed and the schema is complete once.
2. `cargo test -p orca-runtime --lib --locked` has zero failures under the
   CI profile (`nextest` ci, 2 threads, retries) on consecutive runs, and the
   previous 46-57 default-parallelism failures are gone (goal-store and
   ORCA_HOME isolation).
3. `cargo test -p orca-runtime --test runtime_host --locked` passes with
   default parallelism on 5 consecutive runs (the previous flake rate was
   1/43; 5 clean runs plus the lib gate give confidence).
4. Focused goal-store tests still pass; existing goal-store behavior tests
   (schema version, receipts, recovery) are unchanged.
5. `cargo fmt --all -- --check` and `git diff --check` are clean.
6. No new global mutex serializes the lib suite; parallel execution is still
   genuinely parallel (spot-check with `--test-threads=8` timing).
7. `cargo test -p orca-runtime --lib --locked -- --test-threads=16` completes
   without hangs on consecutive runs (previously a subset of `runtime_host`
   surface tests stalled in an unbounded `wait_operation_terminal`).

## Test-Environment Home Override Design (High-Parallelism Hang Fix)

Under `cargo test --test-threads=16` a subset of `runtime_host` surface tests
stalled in an unbounded `wait_operation_terminal` / `host.shutdown()`.
Sample/lsof evidence showed the test thread parked in `wait_operation_terminal`
recv while the host thread parked in tokio: the stall was a test-env lock
deadlock, not host starvation. The deadlock chain:

1. `with_orca_home` (controller/session/server) redirected `ORCA_HOME` to a
   per-test temp dir and held the **exclusive** test-env lock for the whole
   closure.
2. Server/`handle_line` and surface tests run their turns on background host
   worker threads. Every `orca_home()` read in the test build went through
   `read_test_orca_home()`, which took the shared **read** lock.
3. A worker blocked on the read lock while the closure (holding the write
   lock) waited for the worker's terminal → deadlock. The v0.3.13 baseline
   shows the same stalls, so this predates the slice; the fix lands here.

The redesign that removes the deadlock, the writer-preference starvation, and
the cross-test home races:

- `ORCA_HOME` is set **once** to a process-wide isolated home
  (`isolated_test_orca_home_dir`, created once, never removed) and is
  otherwise never mutated. All lib tests resolve the same live directory, so
  session/goal/task data (keyed by session or task identity) coexists safely.
- Per-test private homes are **thread-local overrides** installed in
  `orca_core::config::folder_trust` (`install_test_orca_home` /
  `install_host_orca_home`). `config_dir()`, `is_trusted()`, `orca_home()`
  (all four runtime read points), and workflow-script resolution consult the
  override first, so folder trust and workflow launch see the same private
  home as sessions, goals, and task sessions — without ever mutating the
  environment.
- `with_orca_home` (controller/session/server) and
  `with_redirected_orca_home` scope the calling thread's override
  (`with_test_orca_home` / `redirect_test_orca_home` guard, panic-safe).
  Hosts started inside such a scope capture the caller's override at
  `RuntimeHost::start_inner` and install it on the supervisor thread, the
  tokio worker threads, and the blocking pool (`Builder::on_thread_start`
  covers blocking threads too), so a host keeps resolving its private home
  for its whole lifetime while every other test resolves the process-wide
  home.
- `lock_test_env()` remains a plain mutex that serializes tests sharing
  process-wide state (the session index, task registry index); home reads
  never take it, so a test holding it while waiting for its own host threads
  can never deadlock against them.
- Tests that need a private home (surface goal tests, workflow fixtures,
  sandbox/`handle_line` tests, the FIFO `session_listing` test, task registry
  migrations) use never-removed unique subdirectories allocated by an atomic
  counter (`isolated_test_orca_home_subdir`) or per-test temp dirs under the
  redirect guard. This also removed `remove_var` poisons that left `ORCA_HOME`
  unset (falling back to the real `~/.orca`) and temp-dir deletion races
  where a concurrent reader resolved a deleted directory.
- Global test injections that can cross tests are keyed by session
  (`TYPED_PROVIDER_OUTCOME_WRITE_FAILURES`) so parallel tests never consume
  each other's injection budget, and two load-sensitive tests now poll for
  actor state instead of racing a 20 ms budget (`goal_actor_request_times_out_
  with_typed_error`) or an immediate probe (`capability_loss_append_failure_
  retries_under_sustained_command_traffic`).

Remaining shared-state hotspots are serialized by the goal-store
`initialize_schema_fenced` cross-process lock, the task-registry index lock,
and the `lock_test_env` mutex; nothing holds the write side of any env lock
across hosted work, and the environment variable is never redirected.

## Rollback And Deletion

Each change is a small reversible commit. Reverting the goal-store lock
restores concurrent-open contention (the pre-slice state). Reverting the test
ORCA_HOME helper restores shared-home lib tests. Reverting the task-registry
index-lock narrowing restores the global serialization of task writes without
changing persisted semantics. No data migration is involved. The temporary
cross-process lock file is removed when the goal store is not being opened; it
lives beside the database as a sibling `.lock` file following the
task-registry precedent.
