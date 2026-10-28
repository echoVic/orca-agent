"""Axis A: is the failing set time-bound or capability-bound?

Takes the non-pass set of a Terminal-Bench run, re-runs each task with a multiplied agent timeout,
and reports the smallest multiplier at which the task passes. A task that passes at 4x but not at 1x
is `time_bound`; one that fails at every multiplier is a `capability_gap`.

Planning is free; execution calls Harbor (`--execute`) and costs API spend.
"""

from __future__ import annotations

import glob
import json
import os
import subprocess
from pathlib import Path

REPO = Path(__file__).resolve().parents[4]
MUSL = REPO / "target" / "x86_64-unknown-linux-musl" / "release"


def failing_tasks(run_dirs: list[Path]) -> list[str]:
    """Task names without a reward of 1.0 in any given Harbor run directory."""
    tasks: list[str] = []
    for run_dir in run_dirs:
        matches = glob.glob(str(Path(run_dir) / "*" / "result.json"))
        if not matches:
            matches = glob.glob(str(Path(run_dir) / "*" / "*" / "result.json"))
        for result_path in matches:
            result = json.loads(Path(result_path).read_text(encoding="utf-8"))
            reward = (result.get("verifier_result") or {}).get("rewards", {}).get("reward")
            task = result.get("task_name") or Path(result_path).parent.name.split("__")[0]
            if reward != 1.0 and task not in tasks:
                tasks.append(task.split("/")[-1])
    return sorted(tasks)


def plan(tasks: list[str], multipliers: list[float]) -> list[dict]:
    return [{"task": task, "multiplier": multiplier} for task in tasks for multiplier in multipliers]


def harbor_command(tasks: list[str], multiplier: float, out: Path, attempts: int = 1, concurrency: int = 4) -> list[str]:
    command = [
        "harbor", "run", "-d", "terminal-bench/terminal-bench-2",
        "--agent", "terminal_bench.orca_agent:OrcaInstalledAgent",
        "-k", str(attempts), "-n", str(concurrency),
        "--timeout-multiplier", str(multiplier),
        "--agent-timeout-multiplier", str(multiplier),
    ]
    for task in tasks:
        command += ["--include-task-name", f"terminal-bench/{task}"]
    return command + [
        "-o", str(out),
        "--mounts", json.dumps([
            {"type": "bind", "source": str(MUSL), "target": "/mnt/orca-bin", "read_only": True}
        ]),
    ]


def execute(tasks: list[str], multipliers: list[float], out: Path, attempts: int = 1, concurrency: int = 4) -> list[dict]:
    """Run one Harbor job per multiplier over all tasks; returns `{task, multiplier, outcome, ...}` rows."""
    rows: list[dict] = []
    for multiplier in multipliers:
        run_out = out / f"x{multiplier:g}"
        command = harbor_command(tasks, multiplier, run_out, attempts, concurrency)
        (out / "commands.log").open("a", encoding="utf-8").write(" ".join(command) + "\n")
        subprocess.run(command, capture_output=True, text=True, timeout=6 * 3600)
        rows.extend(_read_results(multiplier, run_out))
    (out / "budget-rows.json").write_text(json.dumps(rows, indent=2) + "\n", encoding="utf-8")
    return rows


def _read_results(multiplier: float, run_dir: Path) -> list[dict]:
    rows = []
    for result_path in glob.glob(str(run_dir / "*" / "result.json")) + glob.glob(str(run_dir / "*" / "*" / "result.json")):
        result = json.loads(Path(result_path).read_text(encoding="utf-8"))
        reward = (result.get("verifier_result") or {}).get("rewards", {}).get("reward")
        exception = (result.get("exception_info") or {}).get("exception_type")
        task = (result.get("task_name") or Path(result_path).parent.name.split("__")[0]).split("/")[-1]
        rows.append(
            {
                "task": task,
                "multiplier": multiplier,
                "outcome": "pass" if reward == 1.0 else "fail",
                "reward": reward,
                "exception": exception,
            }
        )
    return rows
