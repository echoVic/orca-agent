"""Metrics: Wilson intervals, a two-parameter logistic fit and the L50 ceiling estimate.

The logistic is fitted by Newton iterations on the logit (no numpy). With few rungs the fit is used
only for the point estimate, and the report always shows the raw per-rung pass rate next to it.
"""

from __future__ import annotations

import math

Z_95 = 1.959963984540054


def wilson(successes: int, trials: int, z: float = Z_95) -> tuple[float, float]:
    """Wilson score interval for a binomial proportion; returns (low, high)."""
    if trials <= 0:
        return (0.0, 1.0)
    p = successes / trials
    denominator = 1 + z * z / trials
    centre = (p + z * z / (2 * trials)) / denominator
    margin = z * math.sqrt(p * (1 - p) / trials + z * z / (4 * trials * trials)) / denominator
    return (max(0.0, centre - margin), min(1.0, centre + margin))


def _logit_fit(points: list[tuple[float, int, int]]) -> tuple[float, float] | None:
    """Newton fit of logit(p) = a + b*x over (x, successes, trials). Returns (a, b) or None."""
    if len(points) < 2:
        return None
    a, b = 0.0, 0.0
    for _ in range(100):
        ga = gb = haa = hab = hbb = 0.0
        for x, successes, trials in points:
            p = 1.0 / (1.0 + math.exp(-(a + b * x)))
            p = min(max(p, 1e-9), 1 - 1e-9)
            w = trials * p * (1 - p)
            r = successes - trials * p
            ga += r
            gb += r * x
            haa += -w
            hab += -w * x
            hbb += -w * x * x
        det = haa * hbb - hab * hab
        if abs(det) < 1e-12:
            return None
        step_a = (hbb * ga - hab * gb) / det
        step_b = (haa * gb - hab * ga) / det
        # Damped Newton keeps the first iterations from diverging on near-separable data.
        damping = 1.0
        while damping > 0.01:
            na, nb = a - damping * step_a, b - damping * step_b
            if math.isfinite(na) and math.isfinite(nb):
                break
            damping /= 2
        a, b = na, nb
        if abs(step_a) < 1e-8 and abs(step_b) < 1e-8:
            break
    if not (math.isfinite(a) and math.isfinite(b)) or b == 0:
        return None
    return (a, b)


# Conservative DeepSeek-flash prices in USD per million tokens. Overestimates on purpose: the
# cap is a safety limit, not an invoice.
INPUT_USD_PER_MTOK = 0.5
OUTPUT_USD_PER_MTOK = 2.0


def estimate_usd(tokens_in: int | None, tokens_out: int | None) -> float:
    return (
        (tokens_in or 0) / 1_000_000 * INPUT_USD_PER_MTOK
        + (tokens_out or 0) / 1_000_000 * OUTPUT_USD_PER_MTOK
    )


def l50(points: list[tuple[float, int, int]]) -> float | None:
    """Difficulty where the fitted pass probability is 0.5. `points` = (difficulty, pass, attempts)."""
    fit = _logit_fit(points)
    if fit is None:
        return None
    a, b = fit
    return -a / b


def budget_transitions(rows: list[dict]) -> dict:
    """Axis A: map each task to the smallest budget multiplier at which it passes."""
    matrix: dict[str, list[dict]] = {}
    for row in rows:
        matrix.setdefault(row["task"], []).append(row)
    transitions = []
    for task, entries in sorted(matrix.items()):
        entries.sort(key=lambda r: r["multiplier"])
        passed_at = next((e["multiplier"] for e in entries if e["outcome"] == "pass"), None)
        transitions.append(
            {
                "task": task,
                "passed_at": passed_at,
                "failed_at_max": passed_at is None,
                "attempts": entries,
            }
        )
    passed = [t for t in transitions if t["passed_at"] is not None]
    never = [t for t in transitions if t["passed_at"] is None]
    return {
        "tasks": len(transitions),
        "passed_at_some_budget": len(passed),
        "time_bound_share": round(len(passed) / len(transitions), 4) if transitions else None,
        "transitions": transitions,
        "capability_gap_tasks": [t["task"] for t in never],
    }


def cost_per_pass(attempts: list[dict]) -> dict:
    passes = sum(a.get("outcome") == "pass" for a in attempts)
    result = {"passes": passes}
    for key in ("tokens_in", "tokens_out", "cost_usd", "wall_seconds"):
        values = [a[key] for a in attempts if not isinstance(a.get(key), bool) and isinstance(a.get(key), (int, float)) and math.isfinite(a[key]) and a[key] >= 0]
        total = sum(values)
        result[f"{key}_per_pass"] = total / passes if passes and len(values) == len(attempts) else "unknown"
        result[f"{key}_observed_attempts"] = len(values)
        if key == "cost_usd":
            result["observed_cost_usd"] = total
            result["cost_observed_attempts"] = len(values)
    result["cost_basis"] = "observed attempt costs (runtime estimates when emitted); includes failures"
    return result


def observation_stats(attempts: list[dict]) -> dict:
    """Descriptive per-completion-event measurements, including failed attempts."""
    compactions = [event for attempt in attempts for event in attempt.get("compactions", [])
                   if event.get("type") == "context.compacted"]
    providers = [event for attempt in attempts for event in attempt.get("provider_observations", [])]

    def measurements(events, key):
        values = []
        for event in events:
            observation = event.get("observation")
            value = observation.get(key) if isinstance(observation, dict) else None
            if not isinstance(value, bool) and isinstance(value, (int, float)) and math.isfinite(value) and value >= 0:
                values.append(value)
        complete = bool(events) and len(values) == len(events)
        return {"observed_events": len(values), "missing_events": len(events) - len(values),
                "mean": sum(values) / len(values) if complete else "unknown",
                "total": sum(values) if complete else "unknown",
                "observed_mean": sum(values) / len(values) if values else "unknown",
                "observed_total": sum(values) if values else "unknown"}

    compaction = {"completed_events": len(compactions),
                  "attempts_with_records": sum("compactions" in attempt for attempt in attempts)}
    compaction.update({key: measurements(compactions, key) for key in (
        "before_tokens_est", "after_tokens_est", "elapsed_ms", "estimated_cost_usd", "cache_hit_ratio")})
    provider = {"completed_events": len(providers),
                "attempts_with_records": sum("provider_observations" in attempt for attempt in attempts),
                "first_delta_kind_counts": {}}
    provider.update({key: measurements(providers, key) for key in ("elapsed_ms", "first_delta_ms")})
    for event in providers:
        observation = event.get("observation")
        kind = observation.get("first_delta_kind") if isinstance(observation, dict) else None
        kind = kind if isinstance(kind, str) and kind else "unknown"
        provider["first_delta_kind_counts"][kind] = provider["first_delta_kind_counts"].get(kind, 0) + 1
    return {"compaction": compaction, "provider": provider}
