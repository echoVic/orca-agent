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
| C | The worker reaches TaskRegistry terminal state but workflow-host cleanup or actor reaping does not publish the surface terminal. | High | Medium | Confirmed: workflow completion reads the registry through temporarily unavailable actor state. |
| D | Operations terminalize and only host shutdown hangs. | Medium | Low | Rejected: the workflow surface terminal was never observed. |
| E | The surface terminal commits but its waiter is not replayed. | Medium | Low | Rejected: failing attempts report `actor-workflow-completion-error` before any terminal commit. |
| F | A recovered foreign batch advances the cursor after workflow completion prepares its batch. | High | Medium | Confirmed: post-fix runs fail with `CursorRangeAlreadyConsumed`; provider completion already rebuilds after this condition, workflow completion did not. |

## Log Evidence
- PR run `34651520641` x64 attempt 1 timed out at 120 seconds and attempt 2 passed in 0.797 seconds.
- The target had `threads-required = 2` under a two-thread nextest profile, and no peer test ran during the timeout.
- Replacing the nested agent call with a workflow-host timer did not remove the first-attempt timeout.
- Focused run `34655357956` reproduced at stage 14: foreground terminal completed, workflow terminal did not.
- Focused run `34656018054` reproduced at stage 15: TaskRegistry was terminal, but the workflow surface terminal was not published.
- Focused run `34656698367` showed workflow-host cancellation, child termination,
  agent-worker join, and both pipe-reader joins all completing before stage 15.
- Focused run `34657316412` showed the background handle finishing, the
  completion notification being sent, and the actor completing its reap and
  workflow-completion call before stage 15.
- Focused runs `34658262069` and `34658264468` reproduced the same failure:
  `actor-workflow-completion-error: failed to start runtime thread: workflow
  task registry record disappeared before completion`.
- Passing attempts on the same commit report `actor-workflow-completion-ok`.
- The foreground generation owns `ThreadActor.state` while active. The
  background completion branch can run before foreground finalization restores
  that state, so `commit_typed_workflow_completion` cannot reliably obtain the
  registry through `self.state`.
- Post-fix runs `34673404902`, `34673405004`, and `34673405551` eliminated the
  registry error but exposed `failed to commit typed workflow completion:
  CursorRangeAlreadyConsumed` while the injected cancellation checkpoint batch
  was being recovered.

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
| 12 | Wait for workflow task-registry terminal state while foreground stays active |
| 13 | Wait for workflow surface terminal while foreground stays active |
| 14 | Release foreground executor |
| 15 | Wait for foreground terminal |
| 16 | Recover and verify ledger |
| 17 | Shut down runtime host |

## Verification Conclusion
The durable control retry, worker stop request, and workflow-host process
cleanup succeed. The actor receives and reaps the completion. The failure is a
state-ownership race: `commit_typed_workflow_completion` reads TaskRegistry
through `self.state`, but an active foreground generation temporarily owns that
state. The workflow's own durable registry record remains present and terminal.

## Post-Fix Verification
- `TypedWorkflowBackground` now owns the `TaskRegistry` handle captured at
  launch, matching the existing typed-provider ownership pattern.
- Workflow completion now waits for a foreign incomplete batch and rebuilds its
  cursor-bound completion or terminal batch after that batch advances the
  surface cursor, matching typed-provider completion semantics.
- The regression test keeps the foreground executor blocked until the workflow
  surface terminal is committed, making the former race deterministic.
- Local post-fix result: 30 consecutive exact-test runs passed with zero
  retries.
- Native Windows x64 post-fix runs `34673808505`, `34673808669`, and
  `34673809086` each passed 20 consecutive exact-test attempts, for 60/60
  total. No attempt reported `CursorRangeAlreadyConsumed` or a watchdog
  timeout.
- Two Windows runs each exercised the expected internal defer path once while
  the injected cancellation batch was still prepared; both rebuilt the
  workflow completion after that batch settled and passed without a test
  framework retry.
