"""Goal-mode long-run episode: drive the TUI's `/goal` surface and count pause/resume cycles.

Goal mode is TUI-only (`docs/goal-mode.md`), so this episode reuses the shared TUI harness: a private
tmux server, a temporary `ORCA_HOME`, the scripted provider, and the real TUI. Each cycle pauses and
resumes the goal, which forces the runtime to admit a new run every time; the outcome file records how
many runs and cycles were observed so the scenario verifier can assert them.
"""

from __future__ import annotations

import json
import os
import sqlite3
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[4]


def _db_counts(db: Path) -> dict:
    if not db.exists():
        return {"goals": 0, "goal_runs": 0}
    con = sqlite3.connect(db)
    try:
        return {
            "goals": len(list(con.execute("select 1 from goals"))),
            "goal_runs": len(list(con.execute("select 1 from goal_runs"))),
        }
    except sqlite3.Error:
        return {"goals": 0, "goal_runs": 0}
    finally:
        con.close()


def _state(db: Path) -> str:
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


def run(scenario, attempt_dir: Path, binary: Path) -> dict:
    eval_dir = REPO / "scripts" / "eval"
    if str(eval_dir) not in sys.path:
        sys.path.insert(0, str(eval_dir))
    import tui_harness  # noqa: PLC0415

    fixture = tui_harness.TuiFixture(binary, attempt_dir)
    db = fixture.home / "goals.sqlite3"
    tmux = tui_harness.Tmux(f"orca-goal-cap-{os.getpid()}")
    tui = tui_harness.Tui(tmux, fixture)
    cycles_expected = int(scenario.difficulty.get("cycles") or scenario.rung)
    outcome: dict = {
        "created": False,
        "cycles_expected": cycles_expected,
        "cycles_done": 0,
        "runs": 0,
        "cleared": False,
        "state": "",
        "db": str(db),
        "error": None,
    }
    try:
        tmux.start()
        tui.start()
        objective = f"capability goal episode {attempt_dir.name}"
        tui.send(f"/goal {objective}")
        created = tui_harness.wait_for(lambda: _db_counts(db)["goals"] == 1, timeout=60)
        outcome["created"] = bool(created)

        runs = _db_counts(db)["goal_runs"]
        for _ in range(cycles_expected):
            tui.send("/goal pause")
            tui_harness.wait_for(lambda: _state(db).startswith('{"status":"paused"'), timeout=30)
            before = _db_counts(db)["goal_runs"]
            tui.send("/goal resume")
            admitted = tui_harness.wait_for(lambda: _db_counts(db)["goal_runs"] > before, timeout=30)
            if not admitted:
                break
            outcome["cycles_done"] += 1
            runs = _db_counts(db)["goal_runs"]

        tui.send("/goal clear")
        outcome["cleared"] = bool(tui_harness.wait_for(lambda: _db_counts(db)["goals"] == 0, timeout=20))
        outcome["runs"] = runs
        outcome["state"] = _state(db)
        (attempt_dir / "pane-final.txt").write_text(tui.capture(), encoding="utf-8")
    except Exception as error:  # noqa: BLE001 - recorded as evidence
        outcome["error"] = f"{type(error).__name__}: {error}"
    finally:
        tui.kill()
        tmux.stop()
        fixture.close()
    (attempt_dir / "goal-outcome.json").write_text(json.dumps(outcome, indent=2) + "\n", encoding="utf-8")
    return outcome
