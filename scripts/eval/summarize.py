#!/usr/bin/env python3
"""Summarize a Harbor run into the evaluation record that is committed to the repo.

Raw trajectories, container logs and per-trial stdout stay out of git (they are large and
environment-specific); what belongs next to the human report is a small, comparable table:
one row per task with its reward, exception, agent exit status, usage and duration.

Usage:
    python3 scripts/eval/summarize.py jobs/full-88-v0432/2026-09-19__23-31-23 \
        --out docs/reports/2026-09-19-tb2-v0432-summary.json \
        --source-commit 5970b40c --model deepseek-v4-pro --attempts 1 --concurrency 6 \
        --exclude terminal-bench/qemu-startup

The run directory is the timestamped job directory Harbor writes
(`<jobs-dir>/<timestamp>/<task>__<id>/`); pass `--artifacts` instead to read the same
layout from an extracted artifact copy.
"""

from __future__ import annotations

import argparse
import datetime as dt
import glob
import json
import os
import sys
from pathlib import Path

SCHEMA_VERSION = 1


def discover_trials(run_dir: Path) -> list[Path]:
    """Harbor writes `<run>/<trial>/result.json`; accept one nesting level more."""
    patterns = [run_dir / "*" / "result.json", run_dir / "*" / "*" / "result.json"]
    seen: dict[str, Path] = {}
    for pattern in patterns:
        for path in sorted(glob.glob(str(pattern))):
            resolved = Path(path).resolve()
            # A job-level result.json lives directly in the run dir; skip it.
            if resolved.parent == run_dir.resolve():
                continue
            seen[str(resolved)] = resolved
    return list(seen.values())


def seconds_between(started: object, finished: object) -> float | None:
    def parse(value: object) -> dt.datetime | None:
        if not isinstance(value, str):
            return None
        try:
            return dt.datetime.fromisoformat(value.replace("Z", "+00:00"))
        except ValueError:
            return None

    start, end = parse(started), parse(finished)
    if start is None or end is None:
        return None
    return round((end - start).total_seconds(), 1)


def agent_metadata(trial_dir: Path) -> dict:
    path = trial_dir / "agent" / "execution_metadata.json"
    if not path.exists():
        return {}
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return {}


def read_trial(result_path: Path) -> dict:
    result = json.loads(result_path.read_text(encoding="utf-8"))
    trial_dir = result_path.parent
    exception = result.get("exception_info") or {}
    metadata = agent_metadata(trial_dir)
    terminal = metadata.get("terminal") or {}
    usage = metadata.get("usage") or {}
    task = result.get("task_name")
    if not task and isinstance(result.get("task_id"), dict):
        task = result["task_id"].get("name")
    return {
        "task": task or trial_dir.name.split("__")[0],
        "trial": trial_dir.name,
        "reward": (result.get("verifier_result") or {}).get("rewards", {}).get("reward"),
        "exception": exception.get("exception_type"),
        "exception_message": (exception.get("exception_message") or "")[:300] or None,
        "agent_exit_code": metadata.get("exit_code"),
        "agent_terminal": terminal.get("status"),
        "trajectory_bytes": metadata.get("trajectory_bytes"),
        "turns": usage.get("turns"),
        "agent_seconds": seconds_between(
            (result.get("agent_execution") or {}).get("started_at"),
            (result.get("agent_execution") or {}).get("finished_at"),
        ),
        "verifier_seconds": seconds_between(
            (result.get("verifier") or {}).get("started_at"),
            (result.get("verifier") or {}).get("finished_at"),
        ),
    }


def summarize(trials: list[dict]) -> dict:
    rewards = [t["reward"] for t in trials if t["reward"] is not None]
    exceptions: dict[str, int] = {}
    for trial in trials:
        if trial["exception"]:
            exceptions[trial["exception"]] = exceptions.get(trial["exception"], 0) + 1
    zeroed = [t for t in trials if t["reward"] == 0.0]
    passed = sum(1 for reward in rewards if reward == 1.0)
    return {
        "trials": len(trials),
        "scored": len(rewards),
        "passed": passed,
        "zeroed": len(zeroed),
        "errored": sum(1 for t in trials if t["exception"]),
        # Harbor's mean divides by every scheduled trial, counting a trial that never
        # produced a reward (infrastructure failure, timeout kill) as zero. Keep that
        # definition so the number stays comparable with the published runs, and report
        # the scored-only mean next to it.
        "mean_reward": round(sum(rewards) / len(trials), 4) if trials else None,
        "mean_reward_scored_only": round(sum(rewards) / len(rewards), 4) if rewards else None,
        "pass_rate_of_all_trials": round(passed / len(trials), 4) if trials else None,
        "exceptions": dict(sorted(exceptions.items(), key=lambda item: -item[1])),
    }


def protocol(args: argparse.Namespace, trials_raw: list[dict], run_dir: Path) -> dict:
    agent = None
    for result_path in discover_trials(run_dir):
        config = json.loads(result_path.read_text(encoding="utf-8")).get("config") or {}
        agent = (config.get("agent") or {}).get("name")
        if agent:
            break
    return {
        "source_commit": args.source_commit,
        "binary": args.binary,
        "harness": args.harness,
        "agent": agent or args.agent,
        "model": args.model,
        "attempts_per_task": args.attempts,
        "concurrency": args.concurrency,
        "excluded_tasks": args.exclude,
        "environment_build_timeout_multiplier": args.build_timeout_multiplier,
        "note": (
            "mean_reward uses every scheduled trial as the denominator (Harbor's own "
            "definition; a trial without a reward counts as 0); mean_reward_scored_only "
            "averages only the trials that produced a verifier reward."
        ),
    }


def markdown_table(trials: list[dict]) -> str:
    lines = [
        "| task | reward | exception | agent exit | terminal | turns | trajectory bytes |",
        "|---|---|---|---|---|---|---|",
    ]
    for trial in sorted(trials, key=lambda t: (t["task"] or "")):
        lines.append(
            "| {task} | {reward} | {exception} | {exit} | {terminal} | {turns} | {bytes} |".format(
                task=trial["task"],
                reward=trial["reward"],
                exception=trial["exception"] or "",
                exit=trial["agent_exit_code"],
                terminal=trial["agent_terminal"] or "",
                turns=trial["turns"],
                bytes=trial["trajectory_bytes"],
            )
        )
    return "\n".join(lines)


def sanitize(value: object) -> object:
    """Replace the local home directory so a committed summary carries no absolute paths."""
    home = str(Path.home())
    if isinstance(value, str):
        return value.replace(home, "~")
    if isinstance(value, list):
        return [sanitize(item) for item in value]
    if isinstance(value, dict):
        return {key: sanitize(item) for key, item in value.items()}
    return value


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("run_dir", help="Harbor run directory (timestamped job directory)")
    parser.add_argument("--out", help="write the summary JSON here")
    parser.add_argument("--markdown", help="also write the per-task markdown table here")
    parser.add_argument("--source-commit", default=None, help="commit the run was built from")
    parser.add_argument("--binary", default="static x86_64-unknown-linux-musl release build")
    parser.add_argument("--harness", default="Harbor 0.20.0")
    parser.add_argument("--agent", default="terminal_bench.orca_agent:OrcaInstalledAgent")
    parser.add_argument("--model", default=None)
    parser.add_argument("--attempts", type=int, default=1)
    parser.add_argument("--concurrency", type=int, default=None)
    parser.add_argument("--build-timeout-multiplier", type=float, default=None)
    parser.add_argument(
        "--exclude", action="append", default=[], help="task excluded from the run (repeatable)"
    )
    args = parser.parse_args()

    run_dir = Path(args.run_dir).resolve()
    if not run_dir.exists():
        print(f"run directory not found: {run_dir}", file=sys.stderr)
        return 2
    result_paths = discover_trials(run_dir)
    if not result_paths:
        print(f"no trial result.json found under {run_dir}", file=sys.stderr)
        return 2

    trials = [read_trial(path) for path in result_paths]
    payload = {
        "schema_version": SCHEMA_VERSION,
        "generated_at": dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "run": os.path.relpath(run_dir, Path.cwd()) if str(run_dir).startswith(str(Path.cwd())) else str(run_dir),
        "protocol": protocol(args, trials, run_dir),
        "totals": summarize(trials),
        "trials": sorted(trials, key=lambda t: (t["task"] or "")),
    }
    payload = sanitize(payload)
    text = json.dumps(payload, indent=2, sort_keys=False)
    if args.out:
        out = Path(args.out)
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(text + "\n", encoding="utf-8")
        print(f"wrote {out}")
    else:
        print(text)
    if args.markdown:
        md = Path(args.markdown)
        md.parent.mkdir(parents=True, exist_ok=True)
        md.write_text(markdown_table(trials) + "\n", encoding="utf-8")
        print(f"wrote {md}")

    totals = payload["totals"]
    print(
        "total: {trials} trials | scored {scored} | passed {passed} | zeroed {zeroed} | "
        "errored {errored} | mean {mean}".format(
            trials=totals["trials"],
            scored=totals["scored"],
            passed=totals["passed"],
            zeroed=totals["zeroed"],
            errored=totals["errored"],
            mean=totals["mean_reward"],
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
