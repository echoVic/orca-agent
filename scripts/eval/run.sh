#!/usr/bin/env bash
# Run one evaluation suite and pin the provenance of the run.
#
# Usage:
#   scripts/eval/run.sh <suite> <job-name> [-n CONCURRENCY] [extra harbor args...]
#
#   suite: terminal-bench (full TB2), or a path to a local dataset directory
#
# Writes jobs/<job-name>/eval-manifest.json with the commit, binary digest,
# harness version, model routing and concurrency, so two runs can be compared
# without guessing what changed (docs/evaluation-plan.md §4).
set -euo pipefail

SUITE="${1:?suite is required}"
JOB="${2:?job name is required}"
shift 2

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

BINARY="target/x86_64-unknown-linux-musl/release/orca"
MOUNTS='[{"type":"bind","source":"'"$REPO_ROOT"'/target/x86_64-unknown-linux-musl/release","target":"/mnt/orca-bin","read_only":true}]'

case "$SUITE" in
  terminal-bench) DATASET="terminal-bench/terminal-bench-2" ;;
  *) DATASET="$SUITE" ;;
esac

CONCURRENCY=3
ARGS=()
while [[ $# -gt 0 ]]; do
  case "$1" in
    -n) CONCURRENCY="$2"; shift 2 ;;
    *) ARGS+=("$1"); shift ;;
  esac
done

BIN_VERSION="$(docker run --rm -v "$REPO_ROOT/target/x86_64-unknown-linux-musl/release:/mnt:ro" \
  alpine:latest /mnt/orca --version 2>/dev/null | awk '{print $2}' || echo unknown)"
BIN_SHA="$(shasum -a 256 "$BINARY" 2>/dev/null | awk '{print $1}' || echo unknown)"

mkdir -p "jobs/$JOB"
python3 - "$JOB" "$SUITE" "$DATASET" "$BIN_VERSION" "$BIN_SHA" "$CONCURRENCY" <<'PY'
import json, os, subprocess, sys, time
job, suite, dataset, version, digest, concurrency = sys.argv[1:7]
def git(*args):
    try:
        return subprocess.run(["git", *args], capture_output=True, text=True, check=True).stdout.strip()
    except Exception:
        return None
manifest = {
    "job": job,
    "suite": suite,
    "dataset": dataset,
    "commit": git("rev-parse", "HEAD"),
    "commit_subject": git("log", "-1", "--pretty=%s"),
    "dirty": bool(git("status", "--porcelain")),
    "binary_version": version,
    "binary_sha256": digest,
    "harbor_version": subprocess.run(["harbor", "--version"], capture_output=True, text=True).stdout.strip(),
    "model_env": {k: os.environ.get(k) for k in ("ORCA_MODEL", "ORCA_BASE_URL") if os.environ.get(k)},
    "n_concurrent": int(concurrency),
    "started_at": time.strftime("%Y-%m-%dT%H:%M:%S"),
}
with open(os.path.join("jobs", job, "eval-manifest.json"), "w", encoding="utf-8") as fh:
    json.dump(manifest, fh, indent=2)
print("manifest:", json.dumps(manifest, indent=2))
PY

harbor run -d "$DATASET" \
  --agent "terminal_bench.orca_agent:OrcaInstalledAgent" \
  --job-name "$JOB" \
  -n "$CONCURRENCY" \
  --environment-build-timeout-multiplier 2.0 \
  --mounts "$MOUNTS" \
  "${ARGS[@]}" \
  -q -y

python3 scripts/eval/triage.py "jobs/$JOB" --json "jobs/$JOB/triage.json"
