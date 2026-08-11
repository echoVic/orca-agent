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

## Known Limit: High-Parallelism Test Hangs (Separate Slice)

Under `cargo test --test-threads=16` a small set of `runtime_host` surface
tests (`older_incomplete_background_completion_cannot_orphan_a_new_transfer`,
`terminal_interactions_scrub_private_resident_state_before_waiter_wake`,
`unavailable_responder_is_one_atomic_batch_and_append_failure_is_invisible`,
and a few others) can stall in an unbounded `wait_operation_terminal`. This
predates this slice: the v0.3.13 baseline shows the same stalls under the same
command, while the CI profile (2 threads, retries) never triggers them.

The root cause is a test-level unbounded wait on a background operation whose
host thread is starved by high parallelism, not the goal-store or ORCA_HOME
changes in this slice. A follow-up slice should (a) give the affected test
helpers a bounded wait (e.g. `SURFACE_TEST_TIMEOUT`) so a stall becomes a
visible failure, and (b) investigate the host-side starvation if the bounded
wait still fires under high parallelism.

## Rollback And Deletion

Each change is a small reversible commit. Reverting the goal-store lock
restores concurrent-open contention (the pre-slice state). Reverting the test
ORCA_HOME helper restores shared-home lib tests. Reverting the task-registry
index-lock narrowing restores the global serialization of task writes without
changing persisted semantics. No data migration is involved. The temporary
cross-process lock file is removed when the goal store is not being opened; it
lives beside the database as a sibling `.lock` file following the
task-registry precedent.
