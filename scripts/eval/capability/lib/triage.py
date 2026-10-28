"""Failure classification and aggregate failure taxonomy.

The order matters: a harness error hides everything after it, an environment failure is not the
agent's fault, a time-bound failure is measured by re-running with a bigger budget (axis A), and only
then does a failure say something about the agent itself. `product_bug` is the only class that becomes
an issue automatically.
"""

from __future__ import annotations

from dataclasses import dataclass, field

CLASSES = (
    "harness",
    "environment",
    "time_bound",
    "product_bug",
    "verification_strictness",
    "capability_gap",
)

# Signals the runtime emits that are never the task's fault. Kept narrow on purpose: a finding here
# is a product issue, so a false positive costs a wasted issue.
PRODUCT_SIGNALS = {
    "exit_137": "the agent process was killed by its own environment (command killed the agent)",
    "trajectory_missing": "the agent left no trajectory although the session ended",
    "no_tool_calls": "the agent claimed completion without calling a single tool",
    "workspace_not_found": "the runtime could not resolve the workspace",
    "panic": "the runtime panicked",
}


@dataclass
class Evidence:
    """What the runner knows about one attempt; filled by the backend."""

    outcome: str                      # pass | fail | error
    verifier_seconds: float | None = None
    wall_seconds: float | None = None
    wall_limit: float | None = None
    agent_exit_code: int | None = None
    agent_terminal: str | None = None
    turns: int | None = None
    tool_calls: int | None = None
    false_completion: bool = False
    harness_error: str | None = None
    environment_error: str | None = None
    product_signals: list[str] = field(default_factory=list)
    verifier_partial: bool = False   # some verifier checks passed
    cross_budget_passed: bool = False  # axis A evidence: passes with a bigger budget


def classify(evidence: Evidence) -> str:
    """Return one class from `CLASSES` for a non-pass attempt."""
    if evidence.harness_error:
        return "harness"
    if evidence.environment_error:
        return "environment"
    if evidence.product_signals:
        return "product_bug"
    if evidence.cross_budget_passed:
        return "time_bound"
    if evidence.wall_limit and evidence.wall_seconds is not None and evidence.wall_seconds >= 0.95 * evidence.wall_limit:
        return "time_bound"
    if evidence.outcome == "pass":
        return "pass"
    if evidence.verifier_partial and evidence.false_completion:
        return "verification_strictness"
    return "capability_gap"


def taxonomy(classes: list[str]) -> dict[str, int]:
    counts = {name: 0 for name in CLASSES}
    for name in classes:
        if name in counts:
            counts[name] += 1
    return counts


def product_findings(attempts: list[tuple[str, str]]) -> list[tuple[str, str]]:
    """(scenario_id, signal) for every attempt classified as `product_bug`."""
    return [(scenario_id, signal) for scenario_id, signal in attempts if signal in PRODUCT_SIGNALS]
