# Computer-use acceptance harness implementation plan

**Goal:** Repeat the same 15 tasks with independent verification, bounded approvals,
durable services and attributable evidence. This change runs no live GUI tasks.

**Spec:** The user brief, the phase-3 baseline report §1, and phase-2 design §10.
**Architecture:** Standard-library Python modules under `scripts/eval/computer_use`;
a small native Swift helper handles macOS metadata, scoped AX and restoration.
The foreground Python runner owns a strictly sequential task lifecycle. tmux
owns long-running services and each fresh TUI process. Recorded sessions and
synthetic inputs test logic; mock provider tests real TUI transport.

## Constraints

- `python3 -I`; no dependencies, real API calls, or live GUI tests during development.
- Fixed `/tmp/orca-cu-baseline`, isolated ORCA_HOME; preserve previous run data.
- Never change HOME, TMPDIR or `~/.orca`; credentials stay only in child memory.
- Fetch and branch from origin/main; commit locally; never push.

## Implementation sequence

1. Write failing tests for task validation, recorded approval panels, app policy,
   cumulative session metrics and synthetic independent verifiers. Implement
   `tasks.json`, `approvals.py`, `metrics.py`, `verify.py`, `fixtures.py`.
2. Test service health, exact session binding, partial JSONL, terminal detection,
   lock/overlay checks and serial lifecycle failures. Implement `services.py`,
   `preflight.py`, native helper, `tui.py`, and a credential-isolating launcher.
3. Implement `run.py`, reports/comparison, evidence and cleanup; document live and
   dry commands. Test report math and error/cleanup paths.
4. Build current release with `env -u NODE_OPTIONS`; run the requested unittest
   discovery and mock-only dry run. Review the whole branch with a fresh reviewer,
   fix material findings, rerun relevant checks and commit.

## Review focus

- Permission panels with different titles or missing canonical targets must not hang.
- Old/partial/ambiguous session files must never be attributed to a new task.
- Timeout, cancellation, signal and cleanup errors must never advance a task early.
- A process or service starting does not prove it remains alive or completed.
- GUI-free/file-only outcomes must not count as successful GUI acceptance.
- Existing user windows must not be read, navigated, closed or quit during cleanup.
