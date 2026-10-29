"""Capability-evaluation runner: discover scenarios, run them, write a report and a scoreboard.

On demand only — nothing here schedules itself. See
`docs/superpowers/specs/2026-10-29-capability-evaluation-design.md` for the axis definitions.

Examples:
    python3 scripts/eval/capability/run.py --list
    python3 scripts/eval/capability/run.py --axis S --profile smoke
    python3 scripts/eval/capability/run.py --scenarios S0-smoke-pass,S0-smoke-fail --record
    python3 scripts/eval/capability/run.py --axis A --from-run jobs/full-88-v0514/2026-10-28__09-12-44
    python3 scripts/eval/capability/run.py --axis A --from-run <dir> --execute --multipliers 2,4
    python3 scripts/eval/capability/run.py --failures last
    python3 scripts/eval/capability/run.py --against last --record
"""

from __future__ import annotations

import argparse
import hashlib
import json
import shutil
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
sys.path.insert(0, str(HERE))

from lib import backend, metrics, report, scenario  # noqa: E402
from axes import axis_a  # noqa: E402

SCENARIO_ROOT = HERE / "scenarios"
DEFAULT_OUT = REPO / "jobs" / "capability"
SCOREBOARD = REPO / "docs" / "reports" / "capability-scoreboard.json"
PROFILES = {"smoke": 1, "standard": 1, "deep": 3}


def binary_sha(path: Path) -> str | None:
    try:
        return hashlib.sha256(path.read_bytes()).hexdigest()
    except OSError:
        return None


def protocol(args, binary: Path) -> dict:
    return {
        "source_commit": subprocess_output(["git", "rev-parse", "--short", "HEAD"]) or "unknown",
        "binary": str(binary),
        "binary_sha256": (binary_sha(binary) or "unknown")[:16],
        "model": "deepseek-flash (effort max)" if (args.provider or "") != "mock" else "mock",
        "host": sys.platform,
    }


def subprocess_output(command: list[str]) -> str | None:
    import subprocess

    try:
        return subprocess.run(command, cwd=REPO, capture_output=True, text=True, timeout=10).stdout.strip() or None
    except (OSError, subprocess.SubprocessError):
        return None


def parse_rungs(text: str) -> tuple[int, int]:
    low, _, high = text.partition("-")
    return int(low), int(high or low)


def run_axis_a(args) -> int:
    out = (Path(args.out) if Path(args.out).is_absolute() else REPO / args.out) / f"A-{time.strftime('%Y%m%d-%H%M%S')}"
    out.mkdir(parents=True, exist_ok=True)
    run_dirs = [Path(item) for item in (args.from_run or [])]
    if not run_dirs:
        print("axis A needs --from-run <harbor run dir> (repeatable)", file=sys.stderr)
        return 2
    tasks = axis_a.failing_tasks(run_dirs)
    if args.tasks:
        wanted = {name.strip() for name in args.tasks.split(",") if name.strip()}
        tasks = [task for task in tasks if task in wanted]
        missing = wanted - set(tasks)
        if missing:
            print(f"note: not in the failing set: {', '.join(sorted(missing))}", file=sys.stderr)
    multipliers = [float(value) for value in (args.multipliers or "2,4").split(",")]
    planned = axis_a.plan(tasks, multipliers)
    (out / "plan.json").write_text(json.dumps(planned, indent=2) + "\n", encoding="utf-8")
    print(f"axis A plan: {len(tasks)} failing tasks x {len(multipliers)} multipliers = {len(planned)} runs")
    for item in planned[:12]:
        print(f"  {item['task']}  x{item['multiplier']:g}")
    if len(planned) > 12:
        print(f"  ... {len(planned) - 12} more")
    if not args.execute:
        print(f"\nplan only (pass --execute to run Harbor). written: {out/'plan.json'}")
        return 0
    print("\nexecuting (this spends API budget)...")
    rows = axis_a.execute(tasks, multipliers, out)
    transitions = metrics.budget_transitions(rows)
    (out / "transitions.json").write_text(json.dumps(transitions, indent=2) + "\n", encoding="utf-8")
    print(f"time-bound: {transitions['passed_at_some_budget']}/{transitions['tasks']} "
          f"({transitions['time_bound_share']}) · capability gaps: {len(transitions['capability_gap_tasks'])}")
    print(f"written: {out/'transitions.json'}")
    return 0


def run_scenarios(args, scenarios: list[scenario.Scenario], binary: Path) -> dict:
    started = time.strftime("%Y-%m-%dT%H:%M:%S")
    run_id = time.strftime("%Y%m%d-%H%M%S")
    out = (Path(args.out) if Path(args.out).is_absolute() else REPO / args.out) / run_id
    out.mkdir(parents=True, exist_ok=True)
    pairs = []
    product_findings: list[tuple[str, str]] = []
    spent_usd = 0.0
    spent_limit = args.max_usd if args.max_usd is not None else (20.0 if args.profile == "deep" else 5.0)
    for item in scenarios:
        if spent_usd >= spent_limit:
            print(f"stop: estimated spend ${spent_usd:.2f} reached the ${spent_limit:.2f} cap", flush=True)
            break
        attempts = []
        for attempt_index in range(args.k):
            attempt_dir = out / "per-scenario" / item.id / f"attempt-{attempt_index + 1}"
            if item.backend != "local":
                print(f"skip {item.id}: backend `{item.backend}` not implemented yet", flush=True)
                continue
            attempt = backend.run_local(item, attempt_dir, binary=binary, provider=args.provider)
            spent_usd += (
                attempt.cost_usd
                if attempt.cost_usd is not None
                else metrics.estimate_usd(attempt.tokens_in, attempt.tokens_out)
            )
            attempts.append(attempt.as_dict())
            if attempt.failure_class == "product_bug":
                product_findings.extend(triage_signals(item.id, attempt))
            print(
                f"{item.id} [{item.axis} L{item.rung}] attempt {attempt_index + 1}: {attempt.outcome}"
                f" ({attempt.failure_class or 'pass'}, {attempt.wall_seconds:.1f}s)",
                flush=True,
            )
        if attempts:
            pairs.append(
                (
                    {
                        "id": item.id,
                        "axis": item.axis,
                        "rung": item.rung,
                        "difficulty": item.difficulty,
                        "difficulty_value": item.difficulty_value,
                    },
                    attempts,
                )
            )
    providers = {item.provider for item in scenarios}
    protocol_block = protocol(args, binary)
    protocol_block["model"] = "mock" if providers == {"mock"} else "deepseek-flash (effort max)"
    run = {
        "schema_version": 1,
        "run_id": run_id,
        "started_at": started,
        "finished_at": time.strftime("%Y-%m-%dT%H:%M:%S"),
        "profile": args.profile,
        "k": args.k,
        "out": str(out),
        "protocol": protocol_block,
        "spend_usd": round(spent_usd, 3),
        "summary": report.summarize(pairs),
        "product_findings": product_findings,
        "scenarios": [
            {
                "id": meta["id"],
                "axis": meta["axis"],
                "rung": meta["rung"],
                "attempts": attempts,
            }
            for meta, attempts in pairs
        ],
    }
    baseline = None
    if args.against:
        root = out.parent
        baseline = report.latest_run(root, exclude=run["run_id"]) if args.against == "last" else _find_run(root, args.against)
        if baseline:
            run["diff"] = report.diff(run, baseline)
    report.write(out, run)
    print(f"\nreport: {out/'report.md'}")
    if args.record:
        report.scoreboard_append(SCOREBOARD, run)
        daily = REPO / "docs" / "reports" / f"{time.strftime('%Y-%m-%d')}-capability-{run_id}.md"
        shutil.copyfile(out / "report.md", daily)
        print(f"recorded: {SCOREBOARD.relative_to(REPO)} and {daily.relative_to(REPO)}")
    return run


def triage_signals(scenario_id: str, attempt: backend.Attempt) -> list[tuple[str, str]]:
    from lib import triage

    return [(scenario_id, signal) for signal in attempt.product_signals if signal in triage.PRODUCT_SIGNALS]


def _find_run(root: Path, run_id: str) -> dict | None:
    for run in report.load_runs(root):
        if run.get("run_id") == run_id:
            return run
    return None


def show_issue(args) -> int:
    root = (Path(args.out) if Path(args.out).is_absolute() else REPO / args.out)
    run = report.latest_run(root)
    if not run:
        print("no runs yet", file=sys.stderr)
        return 2
    entry = next((s for s in run["scenarios"] if s["id"] == args.issue), None)
    if not entry:
        print(f"scenario {args.issue} not in the latest run", file=sys.stderr)
        return 2
    attempts = entry["attempts"]
    first = attempts[0]
    print(f"### {args.issue}\n")
    print(f"- axis: {entry['axis']} rung {entry['rung']}")
    print(f"- outcome: {first.get('outcome')} · class: {first.get('failure_class')}")
    print(f"- wall: {round(first.get('wall_seconds') or 0, 2)}s · verify: {round(first.get('verify_seconds') or 0, 2)}s · "
          f"agent exit: {first.get('agent_exit_code')}")
    for name in ("harness_error", "environment_error", "product_signals"):
        if first.get(name):
            print(f"- {name}: {first[name]}")
    print("\n- reproduce: `python3 scripts/eval/capability/run.py "
          f"--scenarios {args.issue} --profile smoke`")
    print(f"- artifacts: `{run['out']}/per-scenario/{args.issue}/attempt-1/`")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--list", action="store_true", help="list discovered scenarios and exit")
    parser.add_argument("--axis", help="comma-separated axis letters (A..H, S for the sanity fixtures)")
    parser.add_argument("--rungs", help="rung range, e.g. 1-3")
    parser.add_argument("--scenarios", help="comma-separated scenario ids")
    parser.add_argument("--failures", help="`last`: re-run the failing scenarios of the latest run")
    parser.add_argument("--profile", choices=sorted(PROFILES), default="standard")
    parser.add_argument("--k", type=int, default=None, help="attempts per scenario (default from profile)")
    parser.add_argument("--binary", default=str(backend.DEFAULT_BINARY))
    parser.add_argument("--provider", default=None, help="mock | deepseek (default: scenario value)")
    parser.add_argument("--out", default=str(DEFAULT_OUT))
    parser.add_argument("--record", action="store_true", help="append the scoreboard and a dated report")
    parser.add_argument("--max-usd", type=float, default=None,
                        help="stop before the next scenario when the estimated spend exceeds this")
    parser.add_argument("--against", help="`last` or a run id: write a diff against that run")
    parser.add_argument("--from-run", action="append", help="axis A: Harbor run directory to take failures from")
    parser.add_argument("--multipliers", help="axis A: timeout multipliers, e.g. 2,4")
    parser.add_argument("--tasks", help="axis A: restrict to these task names (comma-separated)")
    parser.add_argument("--execute", action="store_true", help="axis A: actually run Harbor (costs API budget)")
    parser.add_argument("--issue", help="print an issue body for a scenario from the latest run")
    args = parser.parse_args()

    discovered = scenario.discover(SCENARIO_ROOT)
    if args.list:
        for item in discovered:
            print(f"{item.axis:1}  L{item.rung}  {item.id:28} backend={item.backend:5} provider={item.provider}")
        return 0
    if args.issue:
        return show_issue(args)

    axes = args.axis.split(",") if args.axis else None
    if axes and "A" in axes:
        return run_axis_a(args)

    binary = Path(args.binary)
    if not binary.exists():
        print(f"binary not found: {binary}", file=sys.stderr)
        return 2
    args.k = args.k or PROFILES[args.profile]

    if args.failures == "last":
        root = args.out if Path(args.out).is_absolute() else REPO / args.out
        latest = report.latest_run(root)
        if not latest:
            print("no previous run", file=sys.stderr)
            return 2
        ids = report.failed_ids(latest)
        print(f"re-running {len(ids)} failing scenario(s) from {latest['run_id']}")
        selected = scenario.select(discovered, ids=ids)
    else:
        selected = scenario.select(
            discovered,
            axes=axes,
            rungs=parse_rungs(args.rungs) if args.rungs else None,
            ids=args.scenarios.split(",") if args.scenarios else None,
        )
    if not selected:
        print("no scenarios selected", file=sys.stderr)
        return 2
    if args.profile == "smoke" and len(selected) > 5:
        print(f"smoke profile covers at most 5 scenarios (selected {len(selected)})", file=sys.stderr)
        return 2
    run_scenarios(args, selected, binary)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
