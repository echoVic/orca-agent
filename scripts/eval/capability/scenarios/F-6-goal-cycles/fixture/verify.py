#!/usr/bin/env python3
"""F-axis verifier: the goal episode must create, cycle and clear the goal."""
import json
import os
import sys

expected = int(os.environ["CAPABILITY_EXPECTED_CYCLES"])
outcome = json.loads(open(os.environ["CAPABILITY_GOAL_OUTCOME"], encoding="utf-8").read())
print(json.dumps(outcome, indent=1))
problems = []
if not outcome.get("created"):
    problems.append("goal was not created")
if outcome.get("cycles_done") != expected:
    problems.append(f"expected {expected} pause/resume cycles, saw {outcome.get('cycles_done')}")
if outcome.get("runs") < 1 + expected:
    problems.append(f"expected at least {1 + expected} runs, saw {outcome.get('runs')}")
if not outcome.get("cleared"):
    problems.append("goal was not cleared")
if outcome.get("error"):
    problems.append(f"episode error: {outcome['error']}")
if problems:
    print("; ".join(problems))
    sys.exit(1)
print(f"ok: {expected} cycles, {outcome.get('runs')} runs")
