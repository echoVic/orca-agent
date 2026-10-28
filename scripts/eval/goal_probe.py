#!/usr/bin/env python3
"""Goal-mode probe: drive the TUI's `/goal` surface and assert the runtime contract.

Goal mode is a TUI/runtime feature (`docs/goal-mode.md`): `/goal` commands are the only entry
point, the runtime owns the state machine, and `$ORCA_HOME/goals.sqlite3` is authoritative. The
probe therefore drives a real TUI in tmux with the scripted mock provider, then asserts on that
database plus the TUI's liveness:

  create      `/goal <objective>` creates the goal row and starts one run with admitted turns
  no-progress `/goal` with a model that never claims a terminal state pauses the goal with the
              no-progress guard instead of looping forever
  pause       `/goal pause` leaves the goal paused by the user
  resume      `/goal resume` admits a fresh run
  clear       `/goal clear` removes the goal from the store
  recovery    the store, not the TUI process, owns the goal across a `--continue` restart

Usage:
    python3 scripts/eval/goal_probe.py [--binary target/release/orca] [--out jobs/eval-goal-probe]
"""

from __future__ import annotations

import argparse
import json
import shutil
import sqlite3
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

from tui_harness import Tmux, Tui, TuiFixture, json_dump, resolve_out, wait_for  # noqa: E402

TABLES = ("goals", "goal_runs", "goal_turns", "goal_intents")


def counts(db: Path) -> dict[str, int]:
    if not db.exists():
        return {table: 0 for table in TABLES}
    con = sqlite3.connect(db)
    try:
        return {table: len(list(con.execute(f"select 1 from {table}"))) for table in TABLES}
    except sqlite3.Error:
        return {table: 0 for table in TABLES}
    finally:
        con.close()


def goal_state(db: Path) -> str:
    if not db.exists():
        return ""
    con = sqlite3.connect(db)
    try:
        rows = list(con.execute("select state from goals"))
    except sqlite3.Error:
        return ""
    finally:
        con.close()
    return rows[0][0] if rows else ""


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/release/orca")
    parser.add_argument("--out", default="jobs/eval-goal-probe")
    args = parser.parse_args()
    if shutil.which("tmux") is None:
        print("skip: tmux is not installed (goal mode is TUI-only)")
        return 0
    binary = Path(args.binary) if Path(args.binary).is_absolute() else (HERE.parent.parent / args.binary)
    out = resolve_out(args.out)
    failures = 0
    fixture = TuiFixture(binary, out)
    db = fixture.home / "goals.sqlite3"
    tmux = Tmux("orca-goal-eval")
    tui = Tui(tmux, fixture)
    try:
        tmux.start()
        tui.start()
        objective = f"goal probe objective {out.name}"

        tui.send(f"/goal {objective}")
        created = wait_for(lambda: counts(db)["goals"] == 1, timeout=45)
        row: list = []
        if created and db.exists():
            con = sqlite3.connect(db)
            try:
                row = list(con.execute("select objective from goals"))
            finally:
                con.close()
        ok = (
            bool(created)
            and bool(row)
            and row[0][0] == objective
            and counts(db)["goal_runs"] >= 1
            and counts(db)["goal_turns"] >= 1
            and tui.alive()
        )
        failures += 0 if ok else 1
        print(
            f"[{'PASS' if ok else 'FAIL'}] create      objective_match={bool(row and row[0][0] == objective)} "
            f"runs={counts(db)['goal_runs']} turns={counts(db)['goal_turns']} alive={tui.alive()}",
            flush=True,
        )
        json_dump(out / "counts-after-create.json", counts(db))
        (out / "pane-after-create.txt").write_text(tui.capture(), encoding="utf-8")

        paused = wait_for(lambda: goal_state(db).startswith('{"status":"paused"'), timeout=120)
        no_progress = bool(paused) and "no_progress" in goal_state(db)
        failures += 0 if no_progress else 1
        print(f"[{'PASS' if no_progress else 'FAIL'}] no-progress  state={goal_state(db)[:120]}", flush=True)
        (out / "state-after-no-progress.txt").write_text(goal_state(db), encoding="utf-8")

        tui.send("/goal pause")
        time.sleep(2)
        pause_ok = goal_state(db).startswith('{"status":"paused"')
        failures += 0 if pause_ok else 1
        print(f"[{'PASS' if pause_ok else 'FAIL'}] pause       state={goal_state(db)[:100]}", flush=True)

        runs_before = counts(db)["goal_runs"]
        tui.send("/goal resume")
        resumed = wait_for(lambda: counts(db)["goal_runs"] > runs_before, timeout=30)
        resume_ok = bool(resumed) and tui.alive()
        failures += 0 if resume_ok else 1
        print(
            f"[{'PASS' if resume_ok else 'FAIL'}] resume      runs {runs_before}->{counts(db)['goal_runs']} "
            f"state={goal_state(db)[:100]} alive={tui.alive()}",
            flush=True,
        )

        tui.send("/goal clear")
        clear_ok = bool(wait_for(lambda: counts(db)["goals"] == 0, timeout=20))
        failures += 0 if clear_ok else 1
        print(f"[{'PASS' if clear_ok else 'FAIL'}] clear       goals={counts(db)['goals']}", flush=True)

        tui.send("/goal recovered objective")
        wait_for(lambda: counts(db)["goals"] == 1, timeout=45)
        runs_before_restart = counts(db)["goal_runs"]
        tui.kill()
        time.sleep(1)
        tui.start(extra_args=["--continue"])
        tui.send("/goal")
        time.sleep(3)
        recovered = tui.alive() and counts(db)["goals"] == 1
        failures += 0 if recovered else 1
        print(
            f"[{'PASS' if recovered else 'FAIL'}] recovery    goals={counts(db)['goals']} "
            f"runs {runs_before_restart}->{counts(db)['goal_runs']} state={goal_state(db)[:100]} alive={tui.alive()}",
            flush=True,
        )
        (out / "pane-after-recovery.txt").write_text(tui.capture(), encoding="utf-8")
        json_dump(out / "counts-final.json", counts(db))
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
