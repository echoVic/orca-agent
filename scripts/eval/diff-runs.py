#!/usr/bin/env python3
"""Diff two Harbor/Terminal-Bench jobs task by task.

Reports status flips (pass/fail/error), per-task efficiency deltas (turns, tool
calls, wall time, output tokens) and the aggregate picture, so a re-run can be
compared against the baseline the evaluation plan pins.

Usage:
    python3 scripts/eval/diff-runs.py jobs/regression-20260916 jobs/full-89-v0431
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from triage import load_trial  # noqa: E402

RANK = {"PASS": 2, "FAIL": 1, "ERR": 0}


def verdict(trial: dict) -> str:
    if trial["exception"]:
        return "ERR"
    return "PASS" if trial["reward"] == 1.0 else "FAIL"


def collect(job: str) -> dict[str, dict]:
    import glob

    out: dict[str, dict] = {}
    for directory in sorted(glob.glob(job + "/*/")):
        trial = load_trial(directory)
        if trial:
            out[trial["task"]] = trial
    return out


def fmt(value: object, unit: str = "") -> str:
    return "—" if value in (None, 0) else f"{value}{unit}"


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("new")
    parser.add_argument("baseline")
    parser.add_argument("--limit", type=int, default=25)
    args = parser.parse_args()

    new = collect(args.new)
    baseline = collect(args.baseline)
    shared = sorted(set(new) & set(baseline))
    if not shared:
        print("no shared tasks between the two jobs")
        return 1

    regressed, improved, same = [], [], []
    for task in shared:
        old, now = verdict(baseline[task]), verdict(new[task])
        if old == now:
            same.append(task)
        elif RANK[now] < RANK[old]:
            regressed.append((task, old, now))
        else:
            improved.append((task, old, now))

    print(f"# {args.new} vs {args.baseline}")
    print(f"shared tasks: {len(shared)}")
    print(f"regressed: {len(regressed)} · improved: {len(improved)} · unchanged: {len(same)}")
    print()
    if regressed:
        print("## Regressed")
        for task, old, now in regressed[: args.limit]:
            detail = new[task]["exception"] or ""
            print(f"- `{task}`: {old} → {now} {detail}")
        print()
    if improved:
        print("## Improved")
        for task, old, now in improved[: args.limit]:
            print(f"- `{task}`: {old} → {now}")
        print()

    deltas = []
    for task in shared:
        old, now = baseline[task], new[task]
        if old["turns"] and now["turns"]:
            deltas.append((now["turns"] - old["turns"], task, old["turns"], now["turns"],
                           old["tool_calls"], now["tool_calls"], old["wall_s"], now["wall_s"],
                           old["tokens_out"], now["tokens_out"]))
    if deltas:
        deltas.sort(reverse=True)
        print("## Efficiency deltas (new − baseline)")
        print("| task | turns | tool calls | wall s | output tokens |")
        print("|---|---|---|---|---|")
        for _, task, t0, t1, c0, c1, w0, w1, o0, o1 in deltas[: args.limit]:
            print(
                f"| `{task}` | {fmt(t0)} → {fmt(t1)} | {fmt(c0)} → {fmt(c1)} | "
                f"{fmt(w0)} → {fmt(w1)} | {fmt(o0)} → {fmt(o1)} |"
            )
        total_old = sum(d[2] for d in deltas)
        total_new = sum(d[3] for d in deltas)
        print()
        print(f"total turns: {total_old} → {total_new} ({total_new - total_old:+d})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
