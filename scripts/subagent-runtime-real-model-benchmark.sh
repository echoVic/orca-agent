#!/usr/bin/env bash
set -euo pipefail
trap 'exit 130' INT
trap 'exit 143' TERM

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CANDIDATE_BIN="${CANDIDATE_BIN:-$ROOT_DIR/target/debug/orca}"
BASELINE_BIN="${BASELINE_BIN:-/private/tmp/orca-baseline/target/debug/orca}"
BENCH_CWD="${BENCH_CWD:-/private/tmp/orca-baseline}"
AUTH_HOME="${AUTH_HOME:-$HOME/.orca}"
BENCH_MODEL="${BENCH_MODEL:-deepseek-flash}"
BENCH_APPROVAL_MODE="${BENCH_APPROVAL_MODE:-full-auto}"
BENCH_TIMEOUT_SECONDS="${BENCH_TIMEOUT_SECONDS:-600}"
BENCH_ROOT="${BENCH_ROOT:-$(mktemp -d /tmp/orca-subagent-benchmark.XXXXXX)}"
BENCH_VARIANTS="${BENCH_VARIANTS:-baseline candidate}"
BENCH_CASES="${BENCH_CASES:-1 2 3 4 5}"

for path in "$CANDIDATE_BIN" "$BASELINE_BIN"; do
  [[ -x "$path" ]] || { echo "missing executable: $path" >&2; exit 1; }
done
[[ -f "$AUTH_HOME/auth.json" ]] || { echo "missing auth: $AUTH_HOME/auth.json" >&2; exit 1; }
[[ -d "$BENCH_CWD" ]] || { echo "missing benchmark checkout: $BENCH_CWD" >&2; exit 1; }

mkdir -p "$BENCH_ROOT"

prompts=(
  "Read-only architecture review. Inspect these independent areas in this repository: subagent admission, task persistence, continuation recovery, permission inheritance, workflow agent execution, terminal process ownership, result delivery, and TUI task projection. Use parallel subagents where it improves coverage. Return a concise risk table with exact file references and do not edit files."
  "Read-only test investigation. Independently inspect runtime, tools, provider, and TUI tests for missing failure coverage around queued subagents. Use parallel subagents where useful. Return the five highest-value missing tests with exact insertion points and do not edit files."
  "Read-only fault analysis. Trace what happens if the process exits at admission, worker start, first external side effect, terminal receipt, and parent result delivery. Delegate independent crash windows where useful. Return invariants, evidence, and unresolved gaps; do not edit files."
  "Small-task control. Read crates/orca-core/src/subagent_config.rs only and report the configured default subagent depth and concurrency in at most four sentences. Do not edit files and do not delegate unless necessary."
  "Bounded four-way read-only check. Delegate exactly four Explorer tasks in parallel, one for each named file: crates/orca-core/src/subagent_config.rs, crates/orca-core/src/subagent_types.rs, crates/orca-runtime/src/system_prompt.rs, and crates/orca-runtime/src/execution_scope.rs. Each child must read only its assigned file, use no shell, and return at most two claims with exact path:line evidence. Do not attach an output schema; plain text is the requested result. Return one concise four-row table and do not edit files."
)

run_case() {
  local variant="$1" binary="$2" case_index="$3" prompt="$4"
  local case_dir="$BENCH_ROOT/$variant/case-$case_index"
  if [[ -f "$case_dir/exit-code" && -f "$case_dir/duration-seconds" ]]; then
    return
  fi
  if [[ -d "$case_dir" ]]; then
    mv "$case_dir" "$case_dir.interrupted.$(date +%s)"
  fi
  mkdir -p "$case_dir/home"
  cp "$AUTH_HOME/auth.json" "$case_dir/home/auth.json"
  cat > "$case_dir/home/config.toml" <<'EOF'
update_check = false
auto_memory = false
EOF
  local start end status
  start="$(date +%s)"
  set +e
  ORCA_HOME="$case_dir/home" python3 - \
    "$BENCH_TIMEOUT_SECONDS" \
    "$case_dir/events.jsonl" \
    "$case_dir/stderr.txt" \
    "$binary" \
    "$BENCH_CWD" \
    "$BENCH_APPROVAL_MODE" \
    "$BENCH_MODEL" \
    "$prompt" <<'PY'
import os
import pathlib
import signal
import subprocess
import sys
import time

timeout, stdout_path, stderr_path, binary, cwd, approval_mode, model, prompt = sys.argv[1:]
command = [
    binary,
    "exec",
    "--cwd", cwd,
    "--output-format", "jsonl",
    "--save-history",
    "--approval-mode", approval_mode,
    "--model", model,
    prompt,
]
with pathlib.Path(stdout_path).open("wb") as stdout, pathlib.Path(stderr_path).open("wb") as stderr:
    process = subprocess.Popen(
        command,
        stdout=stdout,
        stderr=stderr,
        start_new_session=True,
    )

    def descendant_pids(root_pid):
        rows = subprocess.check_output(
            ["/bin/ps", "-axo", "pid=,ppid="], text=True
        ).splitlines()
        children = {}
        for row in rows:
            pid, parent = (int(value) for value in row.split())
            children.setdefault(parent, []).append(pid)
        descendants = []
        pending = list(children.get(root_pid, []))
        while pending:
            pid = pending.pop()
            descendants.append(pid)
            pending.extend(children.get(pid, []))
        return descendants

    def terminate_tree():
        targets = descendant_pids(process.pid) + [process.pid]
        process_groups = []
        for pid in targets:
            try:
                process_groups.append(os.getpgid(pid))
            except ProcessLookupError:
                pass
        for process_group in reversed(list(dict.fromkeys(process_groups))):
            try:
                os.killpg(process_group, signal.SIGTERM)
            except ProcessLookupError:
                pass
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            pass
        time.sleep(0.1)
        live_groups = []
        for pid in targets:
            try:
                os.kill(pid, 0)
                live_groups.append(os.getpgid(pid))
            except ProcessLookupError:
                pass
        for process_group in reversed(list(dict.fromkeys(live_groups))):
            try:
                os.killpg(process_group, signal.SIGKILL)
            except ProcessLookupError:
                pass
        if process.poll() is None:
            process.wait()

    try:
        raise SystemExit(process.wait(timeout=int(timeout)))
    except subprocess.TimeoutExpired:
        terminate_tree()
        raise SystemExit(124)
    except KeyboardInterrupt:
        terminate_tree()
        raise SystemExit(130)
PY
  status=$?
  set -e
  end="$(date +%s)"
  printf '%s\n' "$status" > "$case_dir/exit-code"
  printf '%s\n' "$((end - start))" > "$case_dir/duration-seconds"
}

for variant in $BENCH_VARIANTS; do
  [[ "$variant" == "baseline" || "$variant" == "candidate" ]] || {
    echo "unknown BENCH_VARIANTS entry: $variant" >&2
    exit 1
  }
  binary="$BASELINE_BIN"
  [[ "$variant" == candidate ]] && binary="$CANDIDATE_BIN"
  for case_number in $BENCH_CASES; do
    [[ "$case_number" =~ ^[1-5]$ ]] || {
      echo "unknown BENCH_CASES entry: $case_number" >&2
      exit 1
    }
    index="$((case_number - 1))"
    run_case "$variant" "$binary" "$case_number" "${prompts[$index]}"
  done
done

python3 - "$BENCH_ROOT" <<'PY'
import csv, json, pathlib, re, sys

root = pathlib.Path(sys.argv[1])
rows = []
for variant in ("baseline", "candidate"):
    if not (root / variant).exists():
        continue
    for case_dir in sorted(
        path for path in (root / variant).glob("case-*")
        if re.fullmatch(r"case-\d+", path.name)
    ):
        events = []
        for line in case_dir.joinpath("events.jsonl").read_text(errors="replace").splitlines():
            try:
                events.append(json.loads(line))
            except json.JSONDecodeError:
                pass
        usage = [e.get("payload", {}) for e in events if e.get("type") == "usage.updated"]
        usage = usage[-1] if usage else {}
        calls = [e.get("payload", {}) for e in events if e.get("type") == "tool.call.requested"]
        subagent_calls = sum(c.get("name") == "subagent" for c in calls)
        statuses = [e.get("payload", {}) for e in events if e.get("type") == "task.status.updated"]
        rows.append({
            "variant": variant,
            "case": case_dir.name,
            "exit": case_dir.joinpath("exit-code").read_text().strip(),
            "seconds": case_dir.joinpath("duration-seconds").read_text().strip(),
            "input_tokens": usage.get("input_tokens", 0),
            "output_tokens": usage.get("output_tokens", 0),
            "cache_tokens": usage.get("cache_tokens", 0),
            "tool_calls": len(calls),
            "subagent_calls": subagent_calls,
            "task_updates": len(statuses),
        })

if not rows:
    raise SystemExit("benchmark produced no case results")
with root.joinpath("metrics.csv").open("w", newline="") as handle:
    writer = csv.DictWriter(handle, fieldnames=rows[0].keys())
    writer.writeheader()
    writer.writerows(rows)
print(f"BENCH_ROOT={root}")
for row in rows:
    print(" ".join(f"{key}={value}" for key, value in row.items()))
PY
