#!/usr/bin/env python3
"""E-axis verifier: the fan-out run must exist, be completed, and cover every agent."""
import json
import os
import subprocess
import sys

expected = int(os.environ["CAPABILITY_EXPECTED_AGENTS"])
binary = os.environ["CAPABILITY_ORCA_BIN"]
result = subprocess.run([binary, "workflow", "list"], capture_output=True, text=True, timeout=60)
try:
    runs = json.loads(result.stdout or "[]")
except json.JSONDecodeError:
    print(f"workflow list is not JSON: {result.stdout[:200]} {result.stderr[:200]}")
    sys.exit(2)
cwd = os.getcwd()
local = [run for run in runs if run.get("cwd") == cwd]
if not local:
    print("no workflow run recorded for this workspace")
    sys.exit(1)
run = local[-1]
if run.get("status") != "completed":
    print(f"run {run.get('runId')} status={run.get('status')} error={run.get('error')}")
    sys.exit(1)
agents = run.get("totalAgentCount") or 0
if agents < expected:
    print(f"expected {expected} agents, run recorded {agents}")
    sys.exit(1)
print(f"ok: {agents} agents, status=completed")
