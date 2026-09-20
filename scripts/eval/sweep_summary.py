#!/usr/bin/env python3
"""Summarise an L1 sweep directory.

Reads the per-suite logs written by `scripts/eval/sweep.sh` and produces a
checkpoint table: PASS/FAIL counts, the suite's own verdict line, and any
"blocked by #NN" annotations so known issues are separated from real failures.

Usage:
    python3 scripts/eval/sweep_summary.py jobs/eval-sweep/20260917-010227
"""

from __future__ import annotations

import argparse
import re
from pathlib import Path


def summarise(log: Path) -> dict:
    text = log.read_text(encoding="utf-8", errors="replace")
    passes = len(re.findall(r"^\[PASS\]", text, re.M))
    failures = len(re.findall(r"^\[FAIL\]", text, re.M))
    verdict = next(
        (line.strip() for line in text.splitlines() if line.strip().startswith("verdict:")),
        "",
    )
    annotations = re.findall(r"(?:blocked by|known issue) #(\d+)|#(\d+)[:)]", text)
    annotations = [a or b for a, b in annotations]
    info = len(re.findall(r"^\[INFO\]", text, re.M))
    return {
        "suite": log.stem.replace("__", " ").replace("_", ".").strip("."),
        "pass": passes,
        "fail": failures,
        "info": info,
        "verdict": verdict,
        "issues": sorted({int(number) for number in annotations}),
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("directory")
    parser.add_argument("--write", action="store_true", help="rewrite summary.md in place")
    args = parser.parse_args()
    directory = Path(args.directory)
    if not directory.is_dir():
        raise SystemExit(f"not a directory: {directory}")

    rows = [summarise(log) for log in sorted(directory.glob("*.log"))]
    if not rows:
        raise SystemExit(f"no logs under {directory}")

    total_pass = sum(r["pass"] for r in rows)
    total_fail = sum(r["fail"] for r in rows)
    known = sorted({issue for r in rows for issue in r["issues"]})

    lines = [
        f"# L1 sweep checkpoint — {directory.name}",
        "",
        f"- checks: **{total_pass} pass** / {total_fail} fail",
        f"- suites: {len(rows)}",
        f"- failures traceable to issues: {', '.join(f'#{n}' for n in known) or 'none'}",
        "",
        "| suite | pass | fail | info | verdict | issues |",
        "|---|---|---|---|---|---|",
    ]
    for row in rows:
        issues = ", ".join(f"#{n}" for n in row["issues"]) or "—"
        lines.append(
            f"| `{row['suite']}` | {row['pass']} | {row['fail']} | {row['info']} | "
            f"{row['verdict'] or '—'} | {issues} |"
        )
    lines += ["", f"Raw logs: `{directory}`", ""]
    summary = "\n".join(lines)
    print(summary)
    if args.write:
        (directory / "summary.md").write_text(summary, encoding="utf-8")
        print(f"wrote {directory / 'summary.md'}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
