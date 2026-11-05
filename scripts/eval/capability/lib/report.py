"""Run assembly, summary, markdown report, scoreboard and baseline diff."""

from __future__ import annotations

import json
import re
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
            "outcome_variance": round((passes / trials) * (1-passes / trials) * trials / (trials-1), 6) if trials > 1 else "unknown",
            "false_completions": sum(bool(a.get("false_completion")) for a in attempts),
            "discovered_attempts": sum(bool(a.get("discoveries")) for a in attempts),
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
    # Recompute from source records; saved/caller-supplied aggregation may be stale.
    # Use the same key and controlled-mock normalization as repeated-run pooling.
    def comparable_groups(run):
        groups = {}
        for group in aggregate_runs([run])["groups"]:
            groups.setdefault(group["scenario_id"], []).append(group)
        return groups
    current_groups, baseline_groups = comparable_groups(current), comparable_groups(baseline)
    def known(group):
        config = group["configuration"]
        required = ("source_commit", "binary_sha256", "requested_model", "reasoning_effort",
                    "backend", "provider", "endpoint_comparison_identity", "scenario_sha256",
                    "prompt_sha256", "fixture_sha256", "verifier_sha256")
        return (group["manifest_sha256"] and not group["manifest_sha256"].startswith("unknown")
                and group["split"] in {"holdout", "tuning"}
                and group["suite_id"] not in {None, "", "unknown"}
                and all(config.get(key) not in (None, "", "unknown") for key in required)
                and isinstance(config.get("budget"), dict)
                and all(config["budget"].get(key) not in (None, "", "unknown") for key in
                        ("max_turns", "max_tool_calls", "max_cost_usd", "max_wall_time_secs")))
    transitions = []
    for scenario_id, scenario in now.items():
        old = before.get(scenario_id)
        if old is None:
            transitions.append({"id": scenario_id, "change": "new"})
            continue
        left, right = baseline_groups.get(scenario_id, []), current_groups.get(scenario_id, [])
        if (len(left) != 1 or len(right) != 1 or not known(left[0]) or not known(right[0])
                or left[0]["comparison_key"] != right[0]["comparison_key"]):
            transitions.append({"id": scenario_id, "change": "incomparable"})
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
                  f"descriptive mean pass rate {run['diff']['mean_pass_rate'][0]} → {run['diff']['mean_pass_rate'][1]}",
                  "Means cover each run's scenarios; they are not a matched improvement estimate. "
                  "Fixed/regressed requires one matching known comparison group per scenario. "
                  "Changed, incomplete or mixed identities are incomparable.", ""]
        for change in run["diff"]["transitions"]:
            lines.append(f"- {change['change']}: `{change['id']}`")
    lines += ["", "## Evidence and repeated attempts", ""]
    for group in run.get("aggregation", {}).get("groups", []):
        lines.append(f"- {group['scenario_id']} ({group['split']}, {group['suite_id']}): "
                     f"{group['passes']}/{group['attempts']} pass; variance {group['outcome_variance']}; "
                     f"95% CI {group['wilson']}; cost/pass {group['cost']['cost_usd_per_pass']}; "
                     f"false completions {group['false_completions']}; discovered attempts {group['discoveries']}")
    for item in run.get("scenarios", []):
        for index, attempt in enumerate(item.get("attempts", []), 1):
            lines.append(f"- {item['id']} attempt {index}: final verifier {attempt.get('final_verification', 'unknown')}; "
                         f"first-discovery evidence {json.dumps(attempt.get('discoveries', []), sort_keys=True)}")
    lines += ["", "## Compression and provider observations", "",
              "Measurements are per emitted completion event across all attempts, including failures. "
              "Means/totals are unknown if any recorded event lacks the field; observed_mean/observed_total "
              "show known subsets with observation counts. No events means unknown measurements, "
              "not proof of zero compactions or provider calls.", "",
              "before_tokens_est/after_tokens_est are local token estimates. prefix_reuse_estimate is "
              "not service KV cache proof; summary usage/cache ratios use returned service metadata when available.", "",
              "Provider elapsed_ms covers the logical provider call, including retry waits; first_delta_ms "
              "is time to the first nonempty streamed delta. first_delta_kind distinguishes reasoning, message "
              "or tool-call deltas (current runtime values: reasoning/text/tool). These are provider measurements, "
              "not task wall time or per-token generation time.", "",
              "Summary estimated_cost_usd is reported separately and not added to cumulative attempt cost. "
              "Cost/pass retains the usage.updated/session.completed cumulative-cost basis; summary expenses "
              "outside that stream are not included. Estimates are not invoices.", ""]
    for group in run.get("aggregation", {}).get("groups", []):
        lines.append(f"- {group['scenario_id']} ({group['split']}, {group['suite_id']}): "
                     f"{json.dumps(group.get('observations', metrics.observation_stats([])), sort_keys=True)}")
    for item in run.get("scenarios", []):
        for index, attempt in enumerate(item.get("attempts", []), 1):
            lines.append(f"- {item['id']} attempt {index}: compactions "
                         f"{json.dumps(attempt.get('compactions', 'unknown'), sort_keys=True)}; "
                         f"provider observations {json.dumps(attempt.get('provider_observations', 'unknown'), sort_keys=True)}")
    lines += ["", "## How to reproduce", "", "```",
              f"python3 scripts/eval/capability/run.py --record --out {run['out']}", "```", ""]
    return "\n".join(lines)


def write(out: Path, run: dict) -> tuple[Path, Path]:
    out.mkdir(parents=True, exist_ok=True)
    (out / "run.json").write_text(json.dumps(run, indent=2) + "\n", encoding="utf-8")
    report = out / "report.md"
    report.write_text(render_markdown(run), encoding="utf-8")
    return out / "run.json", report


def load_runs(root: Path, seen_paths: set | None = None) -> list[dict]:
    runs = []
    seen_paths = set() if seen_paths is None else seen_paths
    for path in sorted(Path(root).glob("*/run.json")):
        resolved = path.resolve()
        if resolved in seen_paths:
            continue
        seen_paths.add(resolved)
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


def aggregate_runs(runs: list[dict]) -> dict:
    """Pool comparable attempts only; never mix tuning with fixed holdout measurements."""
    from . import evidence
    groups, seen = {}, {}
    for run_index, run in enumerate(runs):
        run_id = run.get("run_id")
        # Known IDs identify saved runs. Unknown IDs do not establish sameness.
        if isinstance(run_id, str) and run_id and run_id != "unknown":
            identity = evidence.digest(run)
            if run_id in seen:
                if seen[run_id] != identity:
                    raise ValueError(f"conflicting run id: {run_id}")
                continue
            seen[run_id] = identity
        evaluation = run.get("evaluation", {})
        scenario_ids = set()
        for item in run.get("scenarios", []):
            if item["id"] in scenario_ids:
                raise ValueError(f"duplicate scenario id in run: {item['id']}")
            scenario_ids.add(item["id"])
            for attempt in item.get("attempts", []):
                config = {**run.get("protocol", {}), **attempt.get("protocol", {})}
                meta = next((row for row in evaluation.get("scenarios", []) if row["id"] == item["id"]), evaluation)
                comparison = {key: config.get(key, meta.get("identity", {}).get(key, "unknown")) for key in (
                    "source_commit", "binary_sha256", "requested_model", "reasoning_effort",
                    "observed_requested_model", "observed_reasoning_effort", "backend",
                    "response_model", "provider_fingerprint", "engine_fingerprint", "provider_fixture_sha256",
                    "sampling", "budget", "tool_schema_sha256", "fixture_sha256", "verifier_sha256", "provider",
                    "scenario_sha256", "prompt_sha256", "prepare_sha256", "verifier_command_sha256", "source_fixture_sha256")}
                origin = config.get("endpoint_identity", "unknown")
                fixture = config.get("provider_fixture_sha256", "unknown")
                stable_endpoint = config.get("endpoint_comparison_identity")
                # Only our explicitly identified local mock may normalize an
                # ephemeral port. Real and legacy/unknown fixtures keep origin.
                controlled_mock = (config.get("provider") == "mock" and config.get("backend") == "local"
                    and isinstance(fixture, str) and re.fullmatch(r"[0-9a-f]{64}", fixture)
                    and isinstance(origin, str) and re.fullmatch(r"http://127\.0\.0\.1:[0-9]+", origin)
                    and stable_endpoint == "controlled-mock:" + fixture)
                comparison["endpoint_comparison_identity"] = stable_endpoint if controlled_mock else origin
                split, suite = meta.get("split", "unknown"), meta.get("suite_id", "unknown")
                manifest = evaluation.get("manifest_sha256", "unknown")
                # Unknown manifests do not establish that two run sets are comparable.
                if manifest == "unknown": manifest = "unknown:" + (run_id if isinstance(run_id, str) and run_id and run_id != "unknown" else str(run_index))
                key = evidence.digest([item["id"], split, suite, manifest, comparison])
                group = groups.setdefault(key, {"scenario_id": item["id"], "split": split, "suite_id": suite,
                    "manifest_sha256": manifest, "comparison_key": key, "requested_model": comparison["requested_model"],
                    "configuration": comparison, "endpoint_identities": set(), "run_ids": set(), "rows": []})
                group["run_ids"].add(run.get("run_id", "unknown"))
                group["endpoint_identities"].add(origin)
                group["rows"].append(attempt)
    output = []
    for group in groups.values():
        rows = group.pop("rows")
        n = len(rows); passes = sum(row.get("outcome") == "pass" for row in rows)
        rate = passes / n
        group.update(attempts=n, passes=passes, pass_rate=round(rate, 4),
                     outcome_variance=round(rate * (1-rate) * n / (n-1), 6) if n > 1 else "unknown",
                     wilson=[round(v, 4) for v in metrics.wilson(passes, n)], cost=metrics.cost_per_pass(rows),
                     false_completions=sum(bool(r.get("false_completion")) for r in rows),
                     discoveries=sum(bool(r.get("discoveries")) for r in rows),
                     observations=metrics.observation_stats(rows),
                     run_ids=sorted(group["run_ids"]), endpoint_identities=sorted(group["endpoint_identities"]))
        output.append(group)
    return {"groups": output, "interval": "95% Wilson; descriptive repeated attempts, not deterministic cloud output proof"}
