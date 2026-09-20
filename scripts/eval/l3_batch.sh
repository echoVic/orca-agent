#!/usr/bin/env bash
# L3 batch driver: baseline check -> gold reference -> agent trials, one fix commit at a time.
#
# Usage:
#   scripts/eval/l3_batch.sh [--trials N] [--timeout SECONDS] [--verify-timeout SECONDS] \
#                            [--worktree PATH] [--container NAME] [--build-timeout SECONDS] \
#                            [--force] SHA [SHA...]
#
# Pipeline per commit (steps are skipped when their artifact already exists, so a batch
# resumes cheaply after an interruption):
#   1. `run --baseline-only`  → is this fix testable on this platform at all?
#      (Windows-only fixes and targets that already pass at base stop here)
#   2. `gold`                 → FAIL_TO_PASS / PASS_TO_PASS measured on the fix commit,
#      cached in jobs/eval-repo-tasks/gold/<sha>.json and used by `run` for scoring
#   3. `run` × trials         → agent phase + verification (SWE-bench style when gold exists)
#
# Every step is logged under jobs/eval-l3-batch/<timestamp>/.
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

TRIALS=1
AGENT_TIMEOUT=2700
VERIFY_TIMEOUT=900
BUILD_TIMEOUT=3600
WORKTREE=""
CONTAINER=""
FORCE=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --trials) TRIALS="$2"; shift 2 ;;
    --timeout) AGENT_TIMEOUT="$2"; shift 2 ;;
    --verify-timeout) VERIFY_TIMEOUT="$2"; shift 2 ;;
    --build-timeout) BUILD_TIMEOUT="$2"; shift 2 ;;
    --worktree) WORKTREE="$2"; shift 2 ;;
    --container) CONTAINER="$2"; shift 2 ;;
    --force) FORCE=1; shift ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    -*) echo "unknown option: $1" >&2; exit 2 ;;
    *) break ;;
  esac
done
SHAS=("$@")
if [[ ${#SHAS[@]} -eq 0 ]]; then
  echo "usage: scripts/eval/l3_batch.sh [options] SHA [SHA...]" >&2
  exit 2
fi

OUT="jobs/eval-l3-batch/$(date +%Y%m%d-%H%M%S)"
mkdir -p "$OUT"
COMMON=(--build-timeout "$BUILD_TIMEOUT" --verify-timeout "$VERIFY_TIMEOUT")
[[ -n "$WORKTREE" ]] && COMMON+=(--worktree "$WORKTREE")
[[ -n "$CONTAINER" ]] && COMMON+=(--container "$CONTAINER")
[[ $FORCE -eq 1 ]] && COMMON+=(--force)

latest_result() { # newest result.json for this sha, if any
  ls -td jobs/eval-repo-tasks/"$1"-*/ 2>/dev/null | while read -r dir; do
    [[ -f "$dir/result.json" ]] && { echo "$dir/result.json"; break; }
  done
}

echo "== L3 batch on ${#SHAS[@]} commit(s); logs: $OUT"
for sha in "${SHAS[@]}"; do
  echo
  echo "=== $sha"
  log="$OUT/$sha.log"
  result="$(latest_result "$sha")"

  if [[ -z "$result" ]]; then
    echo "--- baseline-only"
    timeout 7200 python3 scripts/eval/repo_task.py run "$sha" "${COMMON[@]}" --baseline-only \
      >>"$log" 2>&1
    code=$?
    result="$(latest_result "$sha")"
    if [[ $code -ne 0 || -z "$result" ]]; then
      echo "$sha: baseline run failed (exit $code, see $log)" | tee -a "$OUT/summary.txt"
      continue
    fi
  fi

  valid="$(python3 -c "import json,sys;d=json.load(open('$result'));print(d.get('fail_to_pass_on_this_platform', bool(d.get('baseline_failures'))))")"
  if [[ "$valid" != "True" ]]; then
    echo "$sha: no FAIL_TO_PASS on this platform — skipping agent phase" | tee -a "$OUT/summary.txt"
    continue
  fi

  if [[ ! -f "jobs/eval-repo-tasks/gold/$sha.json" ]]; then
    echo "--- gold reference"
    timeout 7200 python3 scripts/eval/repo_task.py gold "$sha" "${COMMON[@]}" >>"$log" 2>&1
    [[ -f "jobs/eval-repo-tasks/gold/$sha.json" ]] || echo "$sha: gold measurement failed (see $log)" | tee -a "$OUT/summary.txt"
  fi

  # A candidate whose base failures all persist at the fix commit has no FAIL_TO_PASS set:
  # running the agent cannot demonstrate anything (e.g. a pre-existing flake that also fails
  # at gold). Check after gold so no agent run is spent on it.
  ftp_count="$(python3 scripts/eval/repo_task.py fail-to-pass "$sha" --count 2>/dev/null || echo 0)"
  if [[ "$ftp_count" == "0" ]]; then
    echo "$sha: no FAIL_TO_PASS after gold — skipping agent phase" | tee -a "$OUT/summary.txt"
    continue
  fi
  echo "$sha: FAIL_TO_PASS = $ftp_count test(s)"

  for trial in $(seq 1 "$TRIALS"); do
    echo "--- agent trial $trial/$TRIALS"
    timeout 7200 python3 scripts/eval/repo_task.py run "$sha" "${COMMON[@]}" \
      --timeout "$AGENT_TIMEOUT" >>"$log" 2>&1
    echo "$sha trial $trial: exit $? (log $log)"
  done
done

echo
echo "== batch report"
python3 scripts/eval/repo_task.py report | tee "$OUT/report.txt"
echo "logs: $OUT"
