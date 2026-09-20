#!/usr/bin/env bash
# Run every L1 suite in sequence and write a checkpoint summary.
#
# Usage: scripts/eval/sweep.sh [output-dir]
#
# Each suite is independent; a failure in one does not stop the sweep. The summary
# records the PASS/FAIL counts plus the "known issue" annotations each suite prints,
# so a checkpoint can be read at a glance.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

OUT="${1:-jobs/eval-sweep/$(date +%Y%m%d-%H%M%S)}"
mkdir -p "$OUT"
SUMMARY="$OUT/summary.md"

SUITES=(
  "fault_injection.py"
  "command_contract.py"
  "exit_code_probe.py"
  "prompt_contract.py"
  "subagent_probe.py"
  "context_probe.py --turns 240"
  "shell_edge_probe.py"
  "signal_probe.py"
  "server_probe.py"
  "mcp_probe.py"
  "wire_param_probe.py"
  "daemon_probe.py"
  "acp_param_probe.py"
  "sandbox_probe.py"
  "sandbox_probe.py --privileged"
  "sandbox_enforcement_probe.py"
  "permission_probe.py"
  "linux_shell_probe.py"
  "trust_probe.py"
  "trust_race_probe.py"
  "workflow_probe.py"
  "workspace_lifetime_probe.py"
  "task_recovery_probe.py"
  "server_restart_probe.py"
  "session_recovery_probe.py"
  "storage_retention_probe.py"
)

{
  echo "# L1 sweep — $(date '+%Y-%m-%d %H:%M')"
  echo
  echo "| suite | pass | fail | exit |"
  echo "|---|---|---|---|"
} > "$SUMMARY"

for entry in "${SUITES[@]}"; do
  name="${entry%% *}"
  args="${entry#"$name"}"
  log="$OUT/$(echo "$name $args" | tr ' /' '__').log"
  echo "=== $name $args"
  # shellcheck disable=SC2086
  timeout 1800 python3 "scripts/eval/$name" $args > "$log" 2>&1
  code=$?
  pass=$(grep -c '^\[PASS\]' "$log" || true)
  fail=$(grep -c '^\[FAIL\]' "$log" || true)
  echo "| \`$name${args:+ $args}\` | $pass | $fail | $code |" >> "$SUMMARY"
  echo "    pass=$pass fail=$fail exit=$code"
done

{
  echo
  echo "## Known-issue annotations"
  echo
  echo '```'
  grep -h "blocked by #" "$OUT"/*.log || echo "(none)"
  echo '```'
  echo
  echo "Raw logs: \`$OUT\`"
} >> "$SUMMARY"

echo
echo "summary: $SUMMARY"
cat "$SUMMARY"
