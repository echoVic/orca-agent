#!/usr/bin/env python3
"""Workflow-mode probe: drive the TUI's `/workflows` and `/workflow:<name>` surface.

The CLI contract is covered by `workflow_probe.py`. This probe covers the *mode* a user meets in
the TUI:

  list        `/workflows` opens the task panel (`Esc` closes it, the TUI stays alive)
  run         `/workflow:<name>` runs a project workflow (`.orca/workflows/<name>.js`): the run is
              persisted with a terminal status (`orca workflow list`), the agent call reaches the
              provider, and the task panel lists the finished task afterwards
  unknown     `/workflow:<missing>` reports a failure in the transcript instead of failing silently

Usage:
    python3 scripts/eval/workflow_mode_probe.py [--binary target/release/orca]
"""

from __future__ import annotations

import argparse
import json
import shutil
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

from tui_harness import Tmux, Tui, TuiFixture, json_dump, resolve_out, wait_for  # noqa: E402

PROJECT_WORKFLOW = """export const meta = { name: 'probe', description: 'workflow mode probe', phases: ['main'] };
const result = await phase('main', async () => agent('FIMODE=clean workflow mode probe'));
export default result;
"""

TERMINAL = ("completed", "failed", "stopped")


def request_count(fixture: TuiFixture) -> int:
    if not fixture.request_log.exists():
        return 0
    return len(fixture.request_log.read_text(encoding="utf-8").splitlines())


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/release/orca")
    parser.add_argument("--out", default="jobs/eval-workflow-mode-probe")
    args = parser.parse_args()
    if shutil.which("tmux") is None:
        print("skip: tmux is not installed (workflow mode is TUI-only)")
        return 0
    binary = Path(args.binary) if Path(args.binary).is_absolute() else (HERE.parent.parent / args.binary)
    out = resolve_out(args.out)
    failures = 0
    fixture = TuiFixture(binary, out, files={".orca/workflows/probe.js": PROJECT_WORKFLOW})
    tmux = Tmux("orca-workflow-eval")
    tui = Tui(tmux, fixture)
    try:
        tmux.start()
        tui.start()

        tui.send("/workflows")
        time.sleep(3)
        pane = tui.capture()
        panel_ok = "Tasks" in pane and tui.alive()
        failures += 0 if panel_ok else 1
        print(f"[{'PASS' if panel_ok else 'FAIL'}] list        task panel opened (Tasks visible={panel_ok})", flush=True)
        (out / "pane-workflows-empty.txt").write_text(pane, encoding="utf-8")
        tui.send("", pause=0)  # focus the panel, then close it
        tui.tmux.call("send-keys", "-t", tui.pane, "Escape")
        time.sleep(1)

        before = request_count(fixture)
        tui.send("/workflow:probe")
        finished = wait_for(
            lambda: any(run.get("status") in TERMINAL for run in fixture.workflow_runs()),
            timeout=180,
        )
        runs = fixture.workflow_runs()
        after = request_count(fixture)
        run_ok = bool(finished) and bool(runs) and tui.alive()
        failures += 0 if run_ok else 1
        print(
            f"[{'PASS' if run_ok else 'FAIL'}] run         runs={[(r.get('workflowName'), r.get('status')) for r in runs]} "
            f"requests {before}->{after} alive={tui.alive()}",
            flush=True,
        )
        json_dump(out / "workflow-runs.json", runs)
        (out / "pane-workflow-run.txt").write_text(tui.capture(), encoding="utf-8")

        model_called = after > before
        completed = any(run.get("status") == "completed" for run in runs)
        stream_ok = model_called and completed and run_ok
        failures += 0 if stream_ok else 1
        print(
            f"[{'PASS' if stream_ok else 'FAIL'}] result      model_called={model_called} completed={completed}",
            flush=True,
        )

        tui.send("/workflows")
        time.sleep(3)
        pane = tui.capture()
        listed_after = ("probe" in pane or "Workflow" in pane) and tui.alive()
        failures += 0 if listed_after else 1
        print(f"[{'PASS' if listed_after else 'FAIL'}] list-after  finished task listed in the panel", flush=True)
        (out / "pane-workflows-after.txt").write_text(pane, encoding="utf-8")
        tui.tmux.call("send-keys", "-t", tui.pane, "Escape")
        time.sleep(1)

        tui.send("/workflow:missing_probe_workflow")
        time.sleep(2)
        pane = tui.capture()
        error_shown = tui.alive() and "not found" in pane.lower() and "RuntimeUnavailable" not in pane
        failures += 0 if error_shown else 1
        cause = next((line.strip() for line in pane.splitlines() if "cause" in line), "")
        print(f"[{'PASS' if error_shown else 'FAIL'}] unknown     error_shown={error_shown} cause={cause[:100]}", flush=True)
        (out / "pane-unknown-workflow.txt").write_text(pane, encoding="utf-8")
        if "RuntimeUnavailable" in pane:
            (out / "unknown-workflow-misleading-cause.txt").write_text(pane, encoding="utf-8")
    except Exception as error:  # noqa: BLE001 - a probe failure is evidence
        failures += 1
        print(f"[FAIL] harness     {type(error).__name__}: {error}", flush=True)
    finally:
        tui.kill()
        tmux.stop()
        fixture.close()
    print(f"\nverdict: {'PASS' if not failures else 'FAIL'}")
    print(f"logs: {out}")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
