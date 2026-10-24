# Computer-use acceptance harness

Run the same S1–S8 and M1–M7 prompts before and after a change. Python uses only
the standard library; run it in isolated mode (`python3 -I`). Requires macOS,
tmux, Xcode command-line tools (`swiftc`), and `target/release/orca`.

```sh
env -u NODE_OPTIONS cargo build --release --locked --bin orca
python3 -I -m unittest discover -s scripts/eval/computer_use -p 'test_*.py'
python3 -I scripts/eval/computer_use/run.py --dry-run --out /tmp/orca-cu-dry-01
```

`--dry-run` creates fixtures, compiles the native helper, reads preflight metadata,
checks recorded approval captures, starts HTTP and caffeinate in separate tmux
windows, and starts **a fresh real Orca TUI for every selected task**. It submits a
fixed harmless prompt to `--provider mock`, requires a persisted terminal, checks
zero tool calls, and waits for process exit. It never reads the API key, invokes
the computer tool, accesses AX trees/screenshots of other apps, sends a
notification, activates an app, moves the pointer, or performs GUI cleanup.
Locked-screen metadata is recorded; a locked screen does not prevent mock smoke.
Mock results are explicitly unverified GUI outcomes, with no live success rate.

Live commands below **operate the Mac**. Run them only during an authorized,
unattended period, with Accessibility and screen capture already granted:

```sh
python3 -I scripts/eval/computer_use/run.py --out /tmp/orca-cu-live-before
python3 -I scripts/eval/computer_use/run.py --tasks S1,M2,M7 --out /tmp/orca-cu-live-subset
python3 -I scripts/eval/computer_use/run.py --baseline /tmp/orca-cu-live-before/aggregates.json --out /tmp/orca-cu-live-after
```

Default output is `.superpowers/sdd/<local-date>-computer-use-eval/<run-id>/`.
Existing output directories are refused, never overwritten. A local advisory
lock and refusal to reuse an existing `tmux -L orca-cu-eval` server prevent two
harness invocations from sharing state. Default selection is all 15, exactly one
attempt per task, in task-file order; `--tasks` rejects unknown or duplicate IDs.
M2 substitutes the server's actual port. M7 explicitly requests TextEdit.

All fixtures, the trusted git workspace and `ORCA_HOME` live under
`/tmp/orca-cu-baseline/`. Existing `fixtures`, `out`, `ws`, `home` and `state` are
moved into `archive/<run-id>/` before setup. Per-task outputs are moved to
`evidence/<task>/previous-outputs/`; draft.txt is reset. The generated uncompressed
PDF has a correct xref and two page objects; PDFKit independently checks its page
count. Auto memory is disabled in the temporary home for reproducibility.
HOME and TMPDIR are preserved. `~/.orca` is never written. Only the live child
exec wrapper reads `DEEPSEEK_API_KEY` from `~/.orca/auth.json`, keeps it in memory,
and injects it into Orca; it never appears in tmux command arguments or reports.

The Python foreground loop owns task advancement. Each task completes this
sequence before another starts: reset → preflight → service health → fresh TUI →
prompt → approvals/terminal → process exit → independent verification → GUI
cleanup. There is no detached scheduler. Timeout sends Esc, then shutdown waits
for exit; an unexited process or failed cleanup stops the suite. SIGINT/SIGTERM
enter the same cleanup path. SIGKILL/power loss cannot run Python cleanup; inspect
the dedicated server and preserved run evidence before a new run.

HTTP uses an ephemeral localhost port, serves only the fixture pages and a
health endpoint, and logs form POST bodies. It and `caffeinate -dimsu` each run
via exec in their own tmux window; pane liveness and HTTP health are checked
before each task and while polling. Both services and the server stop in finally.
Do not use `tmux kill-server` without `-L orca-cu-eval` when investigating a stale run.

Live preflight checks ioreg's lock flag and frontmost loginwindow before every
task and while polling. State snapshots contain bundle IDs/PIDs, pointer position,
CG window IDs/layers/bounds and display bounds, **no window titles**. Layer-1000
windows covering at least 90% of a display are recorded per task and do not cause
a skip (the UU look-through branch remains a live-only check). Preflight never
requests TCC access. Browser setup creates a new window and navigates it to
about:blank, proven by the window title: a start page can list the user's
favorites and history, so setup fails rather than leave one. The task window must
be the only new standard AX window; status bubbles and popups are not. Cleanup
closes only new visible windows of allowed task apps (bubbles without a close
button are left to their owner), and quits only allowed app PIDs absent at task
start. A user window that disappears fails cleanup only if it was visible and at
least 120×60 pt; background apps' helper windows and toasts come and go on their
own. Calculator restores its last display, so the harness presses AC before S1/M4
and those tasks need real input. Final restoration uses the original app PID and
pointer coordinates.

Approval detection reads the current rendered box and the task-bound session
JSONL. Ordinary `Approve · <tool>` comes from `ui.rs`; risk titles are derived
from `ApprovalDialog::title()` in `types.rs` (currently Network Permission
Required, Capability Boundary, Approval Required). Option-bearing approval
boxes with new titles are also detected. Every answer records time, title,
tool, canonical target, options, selected label/key and interaction identity.

- Allowed computer app: **Allow this app this session**. If absent, Allow once
  and `session_option_missing=true` (the baseline's null surface target issue is
  retained as a measured product limitation).
- Allowed secure-field app: Allow typing this once / Allow once.
- Unknown or disallowed computer app: Deny, record a boundary event, stop that task.
- Other tools and permissions: Allow once. No saved/global grants.

Canonical targets come from the committed interaction/semantic request, with
panel preview fallback for the known null-target baseline. Committed typed
interaction IDs deduplicate panels, including permission panels without legacy
approval events. Session attribution excludes every pre-existing file and requires
an exact first persisted user-message/provider match (session titles truncate
at 80 characters); multiple matches fail instead of choosing newest.
Incomplete final JSONL writes are tolerated; malformed complete records fail.
Only persisted session.completed, semantic terminal, or a committed typed
Operation.Terminal proves turn completion.

The app allowlist is an approval policy, **not an OS sandbox** for shell tools or
individual windows. Session grants intentionally reduce repeat approvals and
cannot prevent the model selecting a different window inside the same app.
Do not run against apps containing sensitive material without an appropriately
isolated desktop. This harness does not replace Orca's tool/permission boundaries.
It never intentionally verifies pre-existing document/browser windows. Independent
AX reads of existing Calculator, System Settings and Cherry Studio windows are
limited exceptions required by those tasks; no screenshots of original windows
are taken. Existing browser tabs are not intentionally navigated or closed.

Each task has a file/AX/defaults/sysctl/form-log verifier. Read-task answers must
match independent facts. Appearance requires the selected AX control to agree
with defaults. GUI acceptance also requires successful computer calls with the
right canonical app and task-window identity. M1/M7 require typing, AX document
content matching the saved file, and no file-tool or shell write to that file
(read-only checks such as `cat` are fine); M1's saved lines must have been typed
into the task window. M6 requires three successful GUI clicks. Screenshots
of new task windows supplement AX evidence (System Settings and Cherry Studio
use scoped AX evidence to avoid account/chat screenshots). Chrome shows no web
content to this AX reader, so S2/S8 also OCR the task window screenshot with
Vision and record only whether the expected string appeared. Absence of AX or OCR
evidence is a failure, not inferred visual success. S7 additionally fails click/type/key/scroll calls.
The JSON input to each verifier is retained for review. A model's “done” never
proves success.

`report.md`, `aggregates.json`, `environment.json`, per-task metrics/verification/
cleanup files, session JSONL, pane captures and a SHA-256 manifest are preserved.
Metrics count model responses, tool calls (including validation failures), computer
actions, foreground marks once per result, error messages, approvals, largest
observation text and the **last cumulative** session.usage snapshot. Wall time is
submission to persisted completion; interrupted tasks use their measured stop
time. Costs are Orca estimates, not invoices. Comparisons support the original
baseline schema as well as new per-task aggregates and flag invalid baselines;
compare identical task selections, modes and environments.

`testdata/` contains only cropped approval boxes and a reduced Preview fixture
session from the 2026-10-23 baseline. Conversation system text, reasoning, personal
window titles, screenshot assets and unrelated runtime envelopes are omitted;
session/tool IDs are anonymized. The original baseline harness sources were under
`evidence/harness/` (rather than `harness-src/`). Test expectations use hand-checked
recorded counts. Live raw evidence can contain task-app content: review it before
sharing. No live GUI behavior is certified by dry runs or unit tests.
