# Windows Release Gate Repair Design

## Problem And Classification

GitHub Actions Windows run `31297059121` failed on commit `4fd10e7c1` after the
local release matrix passed. The failures are independently classified:

- `memory::tests::cancellation_after_final_check_rolls_back_existing_memory_file`
  failed three times with `failed to roll back memory: Access is denied. (os
  error 5)`. The existing-file path opens the file with append-only access and
  later calls `set_len`; Windows requires write access for truncation. This is a
  local cross-platform implementation defect in cancellation rollback.
- `headless_max_inner_turns_preserve_trajectory_truth` was terminated after
  120 seconds on all three attempts. The same 128-turn behavior contract takes
  about 50 seconds on macOS, so the generic CI timeout is too narrow for the
  Windows runner. This is a test execution-boundary defect, not a runtime
  behavior failure.

## User Value And Risk

Automatic memory cancellation must leave the user's existing project memory
unchanged on every supported platform. The release must also keep its headless
trajectory proof without converting normal Windows runner variance into three
repeated timeouts that block publication.

## Scope

- Open an existing memory file with the write permission required to truncate
  it during rollback while preserving append semantics.
- Add a nextest timeout override only for the exact 128-turn headless contract.
  The override must remain bounded and must not weaken assertions or retries.
- Preserve the automatic-memory lock, cancellation checks, output protocol,
  transcript format, and the 128-turn product limit.

## Non-Goals

- No change to normal memory note formatting or storage paths.
- No change to agent-loop limits, provider fixture behavior, JSONL events, or
  persisted transcript schemas.
- No global timeout increase and no suppression of Windows failures.

## Ownership And Lifecycle

The memory append function continues to own the file handle while holding the
sidecar exclusive lock. If cancellation wins after the append, that same owner
truncates and flushes the file before releasing the lock. The headless process
continues to own and settle all 128 admitted turns; nextest only owns the
external test deadline.

## Failure Semantics

- Cancellation before write returns `Ok(false)` without changing the file.
- Cancellation after an existing-file append truncates to the original length,
  flushes the rollback, and returns `Ok(false)`.
- Rollback failures remain explicit errors and are not reported as successful
  cancellation.
- The headless contract may run for up to 240 seconds on CI before nextest
  terminates it. Other tests retain the existing 120-second ceiling.

## Compatibility

CLI arguments, TUI workflows, server/JSONL and ACP protocols, memory content,
SQLite data, and persisted transcript formats are unchanged.

## Acceptance

- The Windows RED evidence is the failed GitHub job `93203795993` from run
  `31297059121`.
- `cargo test -p orca-runtime cancellation_after_final_check_rolls_back_existing_memory_file --lib --locked`
- `cargo nextest run --test agent_loop_contract -E 'test(=headless_max_inner_turns_preserve_trajectory_truth)' --locked --profile ci`
- `cargo fmt --all -- --check`
- `git diff --check`
- The full locked workspace gate passes locally.
- A fresh Windows workflow passes both native x64 and ARM64 jobs before the
  release tag is created.

## Rollback

Revert the repair commit. No migration or persisted-data rollback is required.
