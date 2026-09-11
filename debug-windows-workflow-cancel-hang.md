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
| A | A surface command blocks before cancellation is durably retried. | Medium | Low | Rejected: diagnostic run reached the foreground terminal. |
| B | The exact control batch commits but the workflow worker does not observe cancellation. | High | Medium | Rejected: TaskRegistry reached a terminal state. |
| C | The worker reaches TaskRegistry terminal state but workflow-host cleanup or actor reaping does not publish the surface terminal. | High | Medium | Confirmed boundary: diagnostic stage 15. |
| D | Operations terminalize and only host shutdown hangs. | Medium | Low | Rejected: the workflow surface terminal was never observed. |

## Log Evidence
- PR run `34651520641` x64 attempt 1 timed out at 120 seconds and attempt 2 passed in 0.797 seconds.
- The target had `threads-required = 2` under a two-thread nextest profile, and no peer test ran during the timeout.
- Replacing the nested agent call with a workflow-host timer did not remove the first-attempt timeout.
- Focused run `34655357956` reproduced at stage 14: foreground terminal completed, workflow terminal did not.
- Focused run `34656018054` reproduced at stage 15: TaskRegistry was terminal, but the workflow surface terminal was not published.

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
The durable control retry and worker stop request succeed. The remaining hang
is between workflow TaskRegistry terminalization and surface completion
publication. Instrument workflow-host child and reader cleanup next.
