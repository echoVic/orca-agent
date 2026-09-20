#!/usr/bin/env python3
"""Triage a Harbor/Terminal-Bench job directory.

Classifies every trial (pass / fail / error), extracts the error taxonomy, the
agent exit codes, and the efficiency numbers that the evaluation plan tracks
(turns, tool calls, wall time, tokens, budget headroom), then writes a markdown
summary next to a JSON dump.

Usage:
    python3 scripts/eval/triage.py jobs/regression-20260916 [--json out.json]
"""

from __future__ import annotations

import argparse
import collections
import glob
import json
import os
import statistics
from pathlib import Path


def load_trial(directory: str) -> dict | None:
    result_path = os.path.join(directory, "result.json")
    if not os.path.exists(result_path):
        return None
    try:
        result = json.load(open(result_path, encoding="utf-8"))
    except json.JSONDecodeError:
        return None
    name = os.path.basename(directory.rstrip("/"))
    exception = result.get("exception_info") or {}
    rewards = ((result.get("verifier_result") or {}).get("rewards")) or {}
    metadata_path = os.path.join(directory, "agent", "execution_metadata.json")
    metadata = {}
    if os.path.exists(metadata_path):
        try:
            metadata = json.load(open(metadata_path, encoding="utf-8"))
        except json.JSONDecodeError:
            metadata = {}
    trajectory = os.path.join(directory, "agent", "trajectory.jsonl")
    turns = tool_calls = 0
    tokens_in = tokens_out = 0
    status = None
    if os.path.exists(trajectory) and os.path.getsize(trajectory) > 0:
        with open(trajectory, encoding="utf-8", errors="replace") as handle:
            for line in handle:
                if '"turn.started"' in line:
                    turns += 1
                elif '"tool.call.requested"' in line:
                    tool_calls += 1
                elif '"usage.updated"' in line:
                    try:
                        payload = json.loads(line)["payload"]
                    except (json.JSONDecodeError, KeyError):
                        continue
                    tokens_in = payload.get("input_tokens") or tokens_in
                    tokens_out = payload.get("output_tokens") or tokens_out
                elif '"session.completed"' in line:
                    try:
                        status = json.loads(line)["payload"].get("status")
                    except (json.JSONDecodeError, KeyError):
                        pass
    agent_run = result.get("agent_execution") or {}
    wall_s = None
    if agent_run.get("started_at") and agent_run.get("finished_at"):
        from datetime import datetime

        start = datetime.fromisoformat(agent_run["started_at"].replace("Z", "+00:00"))
        end = datetime.fromisoformat(agent_run["finished_at"].replace("Z", "+00:00"))
        wall_s = round((end - start).total_seconds(), 1)
    error_message = (exception.get("exception_message") or "").strip()
    exit_code = None
    for prefix in ("exit ",):
        if error_message.startswith(f"Command failed ({prefix}"):
            exit_code = error_message.split(prefix, 1)[1].split(")", 1)[0]
    return {
        "trial": name,
        "task": (result.get("task_name") or "").split("/")[-1],
        "reward": rewards.get("reward"),
        "exception": exception.get("exception_type"),
        "exception_message": error_message[:200],
        "agent_exit_code": exit_code,
        "session_status": status,
        "turns": turns or None,
        "tool_calls": tool_calls or None,
        "tokens_in": tokens_in or None,
        "tokens_out": tokens_out or None,
        "wall_s": wall_s,
        "trajectory_bytes": os.path.getsize(trajectory) if os.path.exists(trajectory) else 0,
        "reported_exit_code": metadata.get("exit_code"),
        "binary": metadata.get("binary"),
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("job")
    parser.add_argument("--json", default=None)
    args = parser.parse_args()

    trials = [
        t for t in (load_trial(d) for d in sorted(glob.glob(args.job + "/*/"))) if t
    ]
    if not trials:
        print(f"no trials under {args.job}")
        return 1

    passed = [t for t in trials if t["reward"] == 1.0]
    failed = [t for t in trials if t["reward"] == 0.0]
    errored = [t for t in trials if t["exception"]]
    scored = [t for t in trials if t["reward"] is not None]
    kinds = collections.Counter(t["exception"] for t in errored)
    exits = collections.Counter(t["agent_exit_code"] for t in errored if t["agent_exit_code"])
    lost = [t for t in trials if not t["trajectory_bytes"]]

    def median(field: str) -> float | None:
        values = [t[field] for t in trials if t.get(field)]
        return round(statistics.median(values), 1) if values else None

    lines = [
        f"# Triage: {args.job}",
        "",
        f"- trials: {len(trials)} (scored {len(scored)})",
        f"- pass: {len(passed)} · fail: {len(failed)} · error: {len(errored)}",
        f"- accuracy: {(len(passed) / len(scored)):.4f} of scored, "
        f"{len(passed) / len(trials):.4f} of all trials",
        f"- trajectories preserved: {len(trials) - len(lost)}/{len(trials)}",
        f"- median turns: {median('turns')} · tool calls: {median('tool_calls')} · "
        f"wall: {median('wall_s')}s · output tokens: {median('tokens_out')}",
        "",
    ]
    if kinds:
        lines += ["## Errors", ""]
        lines += [f"- {count} × {kind}" for kind, count in kinds.most_common()]
        exit_summary = ", ".join(f"exit {code}: {count}" for code, count in exits.most_common())
        if exit_summary:
            lines.append(f"- agent exits: {exit_summary}")
        lines.append("")
    if errored:
        lines += ["## Errored trials", ""]
        for trial in errored:
            lines.append(
                f"- `{trial['task']}`: {trial['exception']}"
                + (f" (exit {trial['agent_exit_code']})" if trial["agent_exit_code"] else "")
                + f" — {trial['exception_message'][:120]}"
            )
        lines.append("")
    if failed:
        lines += ["## Failed trials (reward 0)", ""]
        lines += [f"- `{t['task']}`" for t in failed]
        lines.append("")
    if lost:
        lines += ["## Trials without a trajectory (evidence lost)", ""]
        lines += [f"- `{t['task']}` ({t['exception'] or 'no exception'})" for t in lost]
        lines.append("")

    report = "\n".join(lines)
    print(report)
    if args.json:
        Path(args.json).write_text(json.dumps(trials, indent=2), encoding="utf-8")
        print(f"\njson: {args.json}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
