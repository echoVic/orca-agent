# Debug Session: windows-workflow-cancel-hang
- **Status**: [OPEN]
- **Issue**: `workflow_cancel_checkpoint_failure_retries_exact_batch_before_signalling_worker` intermittently hangs for 120 seconds on Windows x64 even when nextest runs it without peer tests.
- **Debug Server**: unavailable from the hosted Windows runner; CI stage output is the fallback evidence channel.
- **Log File**: GitHub Actions job log for the focused diagnostic workflow.

## Reproduction Steps
1. Run the exact test repeatedly on a native Windows x64 GitHub runner.
2. Record the last completed stage when an attempt exceeds the bounded diagnostic timeout.

## Hypotheses & Verification
| ID | Hypothesis | Likelihood | Effort | Evidence |
|----|------------|------------|--------|----------|
| A | A surface command blocks before cancellation is durably retried. | Medium | Low | Pending: stage markers around launch, foreground admission, cancellation, and retry observation. |
| B | The exact control batch commits but the workflow worker does not observe cancellation. | High | Medium | Pending: compare committed control with worker/task completion. |
| C | The worker exits but the actor does not reap completion or wake the terminal waiter. | High | Medium | Pending: distinguish terminal waits from shutdown. |
| D | Operations terminalize and only host shutdown hangs. | Medium | Low | Pending: markers before and after host shutdown. |

## Log Evidence
- PR run `34651520641` x64 attempt 1 timed out at 120 seconds and attempt 2 passed in 0.797 seconds.
- The target had `threads-required = 2` under a two-thread nextest profile, and no peer test ran during the timeout.
- Replacing the nested agent call with a workflow-host timer did not remove the first-attempt timeout.

## Diagnostic Stage Codes
| Stage | Next blocking boundary |
|-------|------------------------|
| 2 | Start runtime host |
| 3 | Start runtime thread |
| 4 | Attach and claim surface subscription |
| 5 | Launch workflow |
| 6 | Reserve foreground operation |
| 7 | Admit foreground operation |
| 8 | Observe foreground executor entry |
| 9 | Dispatch cancellation with injected checkpoint failure |
| 10 | Observe exact retry batch |
| 11 | Read snapshot during cancellation |
| 12 | Release foreground executor |
| 13 | Wait for foreground terminal |
| 14 | Wait for workflow task-registry terminal state |
| 15 | Wait for workflow surface terminal |
| 16 | Recover and verify ledger |
| 17 | Shut down runtime host |

## Verification Conclusion
Pending focused Windows stage evidence.
