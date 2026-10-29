"""Run assembly, summary, markdown report, scoreboard and baseline diff."""

from __future__ import annotations

import json
import time
from pathlib import Path

from . import metrics, triage

SCOREBOARD = "capability-scoreboard.json"


def summarize(pairs: list[tuple[dict, list[dict]]]) -> dict:
    """`pairs` = (scenario meta, attempts). Returns per-axis rungs, L50, taxonomy and cost."""
    axes: dict[str, dict] = {}
    classes: list[str] = []
    for meta, attempts in pairs:
        axis = axes.setdefault(meta["axis"], {"rungs": [], "scenarios": []})
        passes = sum(1 for attempt in attempts if attempt["outcome"] == "pass")
        trials = len(attempts)
        entry = {
            "id": meta["id"],
            "rung": meta["rung"],
            "difficulty": meta["difficulty"],
            "difficulty_value": meta.get("difficulty_value"),
            "attempts": trials,
            "passes": passes,
            "pass_rate": round(passes / trials, 4) if trials else None,
            "wilson": [round(value, 4) for value in metrics.wilson(passes, trials)],
            "cost": metrics.cost_per_pass(attempts),
        }
        axis["scenarios"].append(entry)
        for attempt in attempts:
            if attempt.get("failure_class"):
                classes.append(attempt["failure_class"])
    for axis in axes.values():
        rungs = [entry for entry in axis["scenarios"] if entry["attempts"] and entry["difficulty_value"] is not None]
        rungs.sort(key=lambda entry: entry["difficulty_value"])
        axis["rungs"] = [entry["rung"] for entry in rungs]
        rates = [entry["pass_rate"] for entry in rungs]
        # A threshold only exists when the ladder straddles it: all-pass or all-fail data is
        # separable and the maximum-likelihood fit would be meaningless.
        if rungs and (all(rate == 1.0 for rate in rates) or all(rate == 0.0 for rate in rates)):
            axis["l50"] = None
        else:
            axis["l50"] = metrics.l50(
                [(entry["difficulty_value"], entry["passes"], entry["attempts"]) for entry in rungs]
            )
        axis["scenarios"].sort(key=lambda entry: entry["rung"])
    return {"axes": axes, "taxonomy": triage.taxonomy(classes), "scenarios": len(pairs)}


def failed_ids(run: dict) -> list[str]:
    ids = []
    for axis in run["summary"]["axes"].values():
        for entry in axis["scenarios"]:
            if entry["passes"] < entry["attempts"]:
                ids.append(entry["id"])
    return ids


def _scenario_attempts(run: dict) -> dict:
    return {scenario["id"]: scenario for scenario in run["scenarios"]}


def diff(current: dict, baseline: dict) -> dict:
    now, before = _scenario_attempts(current), _scenario_attempts(baseline)
    transitions = []
    for scenario_id, scenario in now.items():
        old = before.get(scenario_id)
        if old is None:
            transitions.append({"id": scenario_id, "change": "new"})
            continue
        old_pass = all(attempt["outcome"] == "pass" for attempt in old["attempts"])
        new_pass = all(attempt["outcome"] == "pass" for attempt in scenario["attempts"])
        if old_pass != new_pass:
            transitions.append({"id": scenario_id, "change": "fixed" if new_pass else "regressed"})
    for scenario_id in before:
        if scenario_id not in now:
            transitions.append({"id": scenario_id, "change": "removed"})
    return {
        "baseline_run": baseline["run_id"],
        "mean_pass_rate": [round(_mean_pass_rate(baseline), 4), round(_mean_pass_rate(current), 4)],
        "transitions": sorted(transitions, key=lambda item: (item["change"], item["id"])),
    }


def _mean_pass_rate(run: dict) -> float:
    scenarios = [entry for axis in run["summary"]["axes"].values() for entry in axis["scenarios"] if entry["attempts"]]
    if not scenarios:
        return 0.0
    return sum(entry["passes"] / entry["attempts"] for entry in scenarios) / len(scenarios)


def render_markdown(run: dict) -> str:
    protocol = run["protocol"]
    lines = [
        f"# Capability evaluation — {run['run_id']}",
        "",
        f"Profile: **{run['profile']}** · started {run['started_at']} · "
        f"{run['summary']['scenarios']} scenarios · k={run['k']}",
        "",
        "## Protocol",
        "",
        "| item | value |",
        "|---|---|",
    ]
    for key, value in protocol.items():
        lines.append(f"| {key} | `{value}` |")
    lines += ["", "## Axes", ""]
    for axis, data in sorted(run["summary"]["axes"].items()):
        lines += [f"### Axis {axis}", "", "| scenario | rung | difficulty | attempts | passes | pass rate | 95% CI |",
                  "|---|---|---|---|---|---|---|"]
        for entry in data["scenarios"]:
            lines.append(
                f"| {entry['id']} | {entry['rung']} | {entry['difficulty_value']} | {entry['attempts']} | "
                f"{entry['passes']} | {entry['pass_rate']} | [{entry['wilson'][0]}, {entry['wilson'][1]}] |"
            )
        if data.get("l50") is not None:
            lines += ["", f"L50 (fitted difficulty at 50 % pass): **{data['l50']:.3f}**"]
        lines.append("")
    lines += ["## Failure taxonomy", "", "| class | count |", "|---|---|"]
    for name, count in run["summary"]["taxonomy"].items():
        if count:
            lines.append(f"| {name} | {count} |")
    findings = run.get("product_findings") or []
    if findings:
        lines += ["", "## Product findings (issue candidates)", ""]
        for scenario_id, signal in findings:
            lines.append(f"- `{scenario_id}`: {signal}")
    if run.get("diff"):
        lines += ["", "## Diff vs baseline", "", f"Baseline: `{run['diff']['baseline_run']}` · "
                  f"mean pass rate {run['diff']['mean_pass_rate'][0]} → {run['diff']['mean_pass_rate'][1]}", ""]
        for change in run["diff"]["transitions"]:
            lines.append(f"- {change['change']}: `{change['id']}`")
    lines += ["", "## How to reproduce", "", "```",
              f"python3 scripts/eval/capability/run.py --record --out {run['out']}", "```", ""]
    return "\n".join(lines)


def write(out: Path, run: dict) -> tuple[Path, Path]:
    out.mkdir(parents=True, exist_ok=True)
    (out / "run.json").write_text(json.dumps(run, indent=2) + "\n", encoding="utf-8")
    report = out / "report.md"
    report.write_text(render_markdown(run), encoding="utf-8")
    return out / "run.json", report


def load_runs(root: Path) -> list[dict]:
    runs = []
    for path in sorted(Path(root).glob("*/run.json")):
        try:
            runs.append(json.loads(path.read_text(encoding="utf-8")))
        except (OSError, json.JSONDecodeError):
            continue
    return sorted(runs, key=lambda run: run.get("started_at") or "")


def latest_run(root: Path, exclude: str | None = None) -> dict | None:
    for run in reversed(load_runs(root)):
        if exclude and run.get("run_id") == exclude:
            continue
        return run
    return None


def scoreboard_append(path: Path, run: dict) -> None:
    entries = []
    if path.exists():
        try:
            entries = json.loads(path.read_text(encoding="utf-8")).get("runs", [])
        except (OSError, json.JSONDecodeError):
            entries = []
    entries.append(
        {
            "run_id": run["run_id"],
            "started_at": run["started_at"],
            "profile": run["profile"],
            "source_commit": run["protocol"].get("source_commit"),
            "binary_sha256": run["protocol"].get("binary_sha256"),
            "model": run["protocol"].get("model"),
            "axes": {
                axis: {
                    "l50": data.get("l50"),
                    "pass_rate": round(
                        sum(entry["passes"] for entry in data["scenarios"])
                        / max(1, sum(entry["attempts"] for entry in data["scenarios"])),
                        4,
                    ),
                }
                for axis, data in run["summary"]["axes"].items()
            },
            "taxonomy": run["summary"]["taxonomy"],
        }
    )
    path.write_text(json.dumps({"schema_version": 1, "runs": entries}, indent=2) + "\n", encoding="utf-8")
