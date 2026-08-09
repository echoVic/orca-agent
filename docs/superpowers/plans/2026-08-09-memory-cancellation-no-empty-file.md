# Automatic Memory Cancellation File Plan

## 1. RED: encode unchanged-target behavior

- Add tests for cancellation with a missing target and with a pre-existing
  empty target.
- Run the focused memory test and observe the missing-target contract failure
  or the existing race-prone behavior before implementation.

## 2. GREEN: stage or roll back under the lock

- Keep the sidecar lock held for the entire operation.
- Use a same-directory temporary file for a missing target and atomic publish.
- Track the original length for existing targets and truncate on cancellation
  observed after the append.
- Remove temporary artifacts on every cancellation/error path.

## 3. Verification and integration

- Run memory, provider-turn, formatting, and diff checks.
- Review for file ownership and compatibility regressions.
- Rebase onto the latest `origin/main`, integrate, and rerun the post-merge
  focused gate.
