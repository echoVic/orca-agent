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
    passes = [a for a in attempts if a.get("outcome") == "pass"]
    if not passes:
        return {"passes": 0, "tokens_out_per_pass": None, "wall_seconds_per_pass": None}
    tokens = [a.get("tokens_out") for a in passes if a.get("tokens_out") is not None]
    wall = [a.get("wall_seconds") for a in passes if a.get("wall_seconds") is not None]
    return {
        "passes": len(passes),
        "tokens_out_per_pass": (sum(tokens) / len(tokens)) if tokens else None,
        "wall_seconds_per_pass": (sum(wall) / len(wall)) if wall else None,
    }
