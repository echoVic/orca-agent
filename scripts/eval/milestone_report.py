#!/usr/bin/env python3
"""Generate a milestone report for a Harbor/Terminal-Bench job.

Aggregates the result of one job, diffs it against a baseline job, and writes a
markdown report (the "每次里程碑深循环" artifact from docs/evaluation-plan.md).
Works on a partially finished job too, so it can be run while a suite is live.

Usage:
    python3 scripts/eval/milestone_report.py jobs/full-89-v0431-fixed \
        --baseline jobs/full-89-v0431 \
        [--out docs/reports/2026-09-17-terminal-bench-fixed.md]
"""

from __future__ import annotations

import argparse
import collections
import glob
import json
import os
import statistics
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from triage import load_trial  # noqa: E402


def collect(job: str) -> dict[str, dict]:
    trials: dict[str, dict] = {}
    for directory in sorted(glob.glob(job + "/*/")):
        trial = load_trial(directory)
        if not trial:
            continue
        trajectory = Path(directory) / "agent" / "trajectory.jsonl"
        tool_seconds = 0.0
        first = last = None
        requested: dict[str, int] = {}
        if trajectory.exists() and trajectory.stat().st_size:
            with open(trajectory, encoding="utf-8", errors="replace") as handle:
                for line in handle:
                    if not line.startswith("{"):
                        continue
                    try:
                        event = json.loads(line)
                    except json.JSONDecodeError:
                        continue
                    stamp = event.get("timestamp_ms")
                    if stamp:
                        first = first or stamp
                        last = stamp
                    kind = event.get("type")
                    payload = event.get("payload") or {}
                    if kind == "tool.call.requested":
                        requested[payload.get("id")] = stamp
                    elif kind == "tool.call.completed":
                        start = requested.get(payload.get("id"))
                        if start and stamp:
                            tool_seconds += (stamp - start) / 1000
        trial["tool_s"] = round(tool_seconds)
        trial["stream_wall_s"] = round((last - first) / 1000) if first and last else trial["wall_s"]
        trial["model_share"] = (
            round(100 * (trial["stream_wall_s"] - tool_seconds) / trial["stream_wall_s"])
            if trial["stream_wall_s"]
            else None
        )
        trials[trial["task"]] = trial
    return trials


def verdict(trial: dict) -> str:
    """Harbor's scoring semantics: the verifier reward decides.

    An exception with reward 1.0 (e.g. the agent was killed at the timeout after
    already satisfying the verifier) is a pass, not an error.
    """
    if trial["reward"] == 1.0:
        return "PASS"
    if trial["reward"] == 0.0:
        return "FAIL"
    return "ERR"


def median(trials: list[dict], field: str) -> float | None:
    values = [t[field] for t in trials if t.get(field)]
    return round(statistics.median(values), 1) if values else None


def render(job: str, baseline: str | None) -> str:
    trials = collect(job)
    if not trials:
        raise SystemExit(f"no trials under {job}")
    ordered = list(trials.values())
    passed = [t for t in ordered if t["reward"] == 1.0]
    scored = [t for t in ordered if t["reward"] is not None]
    errored = [t for t in ordered if t["exception"]]
    preserved = [t for t in ordered if t["trajectory_bytes"] > 0]
    kinds = collections.Counter(t["exception"] for t in errored)
    totals = collections.Counter()
    for trial in ordered:
        totals[trial["exception"] or "scored"] += 1

    lines = [
        f"# Milestone report — `{job}`",
        "",
        f"Generated: {time.strftime('%Y-%m-%d %H:%M')}",
        "",
        "## Summary",
        "",
        f"- trials: {len(ordered)} (scored {len(scored)})",
        f"- pass: **{len(passed)}** · fail: {len(scored) - len(passed)} · error: {len(errored)}",
        f"- accuracy: {len(passed) / len(scored):.4f} of scored, "
        f"{len(passed) / len(ordered):.4f} of all trials",
        f"- trajectories preserved: {len(preserved)}/{len(ordered)}",
        f"- median turns: {median(ordered, 'turns')} · tool calls: {median(ordered, 'tool_calls')} · "
        f"stream wall: {median(ordered, 'stream_wall_s')}s · output tokens: {median(ordered, 'tokens_out')}",
        f"- median model-time share: {median(ordered, 'model_share')}%",
        "",
    ]
    if kinds:
        lines += ["## Error taxonomy", "", "| exception | count | tasks |", "|---|---|---|"]
        for kind, count in kinds.most_common():
            tasks = ", ".join(f"`{t['task']}`" for t in errored if t["exception"] == kind)
            lines.append(f"| {kind} | {count} | {tasks} |")
        lines.append("")

    slowest = sorted(
        [t for t in ordered if t["tokens_out"]], key=lambda t: t["tokens_out"], reverse=True
    )[:10]
    if slowest:
        lines += [
            "## Heaviest trials (output tokens)",
            "",
            "| task | tokens out | turns | stream wall s | model % | reward | exception |",
            "|---|---|---|---|---|---|---|",
        ]
        for trial in slowest:
            lines.append(
                f"| `{trial['task']}` | {trial['tokens_out']} | {trial['turns']} | "
                f"{trial['stream_wall_s']} | {trial['model_share']} | {trial['reward']} | "
                f"{trial['exception'] or '—'} |"
            )
        lines.append("")

    if baseline:
        base = collect(baseline)
        shared = sorted(set(trials) & set(base))
        regressed = [
            (t, verdict(base[t]), verdict(trials[t]))
            for t in shared
            if verdict(trials[t]) != verdict(base[t])
            and verdict(trials[t]) != "PASS"
            and verdict(base[t]) == "PASS"
        ]
        improved = [
            (t, verdict(base[t]), verdict(trials[t]))
            for t in shared
            if verdict(trials[t]) != verdict(base[t])
            and verdict(trials[t]) == "PASS"
        ]
        lines += [
            f"## Against `{baseline}`",
            "",
            f"shared tasks: {len(shared)} · regressed: {len(regressed)} · improved: {len(improved)}",
            "",
        ]
        if regressed:
            lines += ["**Regressed**", ""]
            lines += [
                f"- `{task}`: {old} → {new} ({trials[task]['exception'] or 'verifier failed'})"
                for task, old, new in regressed
            ]
            lines.append("")
        if improved:
            lines += ["**Improved**", ""]
            lines += [f"- `{task}`: {old} → {new}" for task, old, new in improved]
            lines.append("")

    zero = [t["task"] for t in ordered if t["reward"] == 0.0]
    if zero:
        lines += ["## Still scoring 0", "", ", ".join(f"`{t}`" for t in zero), ""]

    lines += [
        "## Artifacts",
        "",
        f"- `{job}/result.json`, per-trial `agent/trajectory.jsonl`, `verifier/test-stdout.txt`",
        f"- regenerate with `python3 scripts/eval/milestone_report.py {job}"
        + (f" --baseline {baseline}" if baseline else "")
        + "`",
        "",
    ]
    return "\n".join(lines)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("job")
    parser.add_argument("--baseline", default=None)
    parser.add_argument("--out", default=None)
    args = parser.parse_args()
    text = render(args.job, args.baseline)
    if args.out:
        Path(args.out).write_text(text, encoding="utf-8")
        print(f"wrote {args.out}")
    else:
        print(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
