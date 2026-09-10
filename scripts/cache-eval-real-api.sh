#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ORCA_BIN="${ORCA_BIN:-$ROOT_DIR/target/debug/orca}"
ORCA_MODEL="${ORCA_MODEL:-deepseek-flash}"
ORCA_EVAL_LIMIT="${ORCA_EVAL_LIMIT:-18000}"
ORCA_EVAL_ROWS="${ORCA_EVAL_ROWS:-700}"
ORCA_EVAL_APPROVAL_MODE="${ORCA_EVAL_APPROVAL_MODE:-plan}"
ORCA_HOME_SOURCE="${ORCA_HOME_SOURCE:-$HOME/.orca}"
ORCA_EVAL_KEEP_DIR="${ORCA_EVAL_KEEP_DIR:-1}"

if [[ ! -x "$ORCA_BIN" ]]; then
  echo "orca binary not found or not executable: $ORCA_BIN" >&2
  echo "Run: cargo build --bin orca" >&2
  exit 1
fi

if [[ ! -f "$ORCA_HOME_SOURCE/auth.json" ]]; then
  echo "missing auth file: $ORCA_HOME_SOURCE/auth.json" >&2
  exit 1
fi

EVAL_ROOT="$(mktemp -d /tmp/orca-cache-eval.XXXXXX)"
EMPTY_CWD="$EVAL_ROOT/empty-cwd"
mkdir -p "$EVAL_ROOT/orca" "$EMPTY_CWD"
cp "$ORCA_HOME_SOURCE/auth.json" "$EVAL_ROOT/orca/auth.json"

cat > "$EVAL_ROOT/orca/config.toml" <<EOF
update_check = false
auto_memory = false

[model_runtime]
auto_compact_token_limit = $ORCA_EVAL_LIMIT
EOF

python3 - "$ORCA_EVAL_ROWS" > "$EVAL_ROOT/blob1.txt" <<'PY'
import sys

rows = int(sys.argv[1])
for i in range(rows):
    print(
        f"alpha-cache-collision row {i:04d}: stable project fact A{i % 17}; "
        "preserve this marker."
    )
PY

python3 - "$ORCA_EVAL_ROWS" > "$EVAL_ROOT/blob2.txt" <<'PY'
import sys

rows = int(sys.argv[1])
for i in range(rows):
    print(
        f"beta-cache-collision row {i:04d}: stable project fact B{i % 19}; "
        "preserve this marker."
    )
PY

python3 - "$ORCA_EVAL_ROWS" > "$EVAL_ROOT/blob3.txt" <<'PY'
import sys

rows = int(sys.argv[1])
for i in range(rows):
    print(
        f"gamma-cache-collision row {i:04d}: stable project fact C{i % 23}; "
        "preserve this marker."
    )
PY

run_orca() {
  local label="$1"
  local mode="$2"
  local prompt="$3"
  local input_file="$4"
  local output_file="$EVAL_ROOT/${label}.jsonl"
  local err_file="$EVAL_ROOT/${label}.stderr"
  local resume_args=()

  if [[ "$mode" == "continue" ]]; then
    resume_args=(--continue)
  fi

  set +e
  if [[ -n "$input_file" ]]; then
    ORCA_HOME="$EVAL_ROOT/orca" ORCA_SUMMARY_DEBUG=1 "$ORCA_BIN" exec \
      --cwd "$EMPTY_CWD" \
      --output-format jsonl \
      --save-history \
      --approval-mode "$ORCA_EVAL_APPROVAL_MODE" \
      --model "$ORCA_MODEL" \
      ${resume_args[@]+"${resume_args[@]}"} \
      "$prompt" \
      < "$input_file" > "$output_file" 2> "$err_file"
  else
    ORCA_HOME="$EVAL_ROOT/orca" ORCA_SUMMARY_DEBUG=1 "$ORCA_BIN" exec \
      --cwd "$EMPTY_CWD" \
      --output-format jsonl \
      --save-history \
      --approval-mode "$ORCA_EVAL_APPROVAL_MODE" \
      --model "$ORCA_MODEL" \
      ${resume_args[@]+"${resume_args[@]}"} \
      "$prompt" \
      < /dev/null > "$output_file" 2> "$err_file"
  fi
  local status=$?
  set -e
  echo "$label exit=$status" >> "$EVAL_ROOT/exits.txt"
}

run_orca \
  turn1-seed \
  new \
  "Cache evaluation. Do not call tools. Reply with one short sentence: alpha received." \
  "$EVAL_ROOT/blob1.txt"

run_orca \
  turn2-trigger-compact \
  continue \
  "Continue the cache evaluation. Do not call tools. Reply with one short sentence: beta received." \
  "$EVAL_ROOT/blob2.txt"

run_orca \
  turn3-after-compact \
  continue \
  "Continue the cache evaluation. Do not call tools. Reply with one short sentence naming the data clusters." \
  ""

run_orca \
  turn4-stability \
  continue \
  "Continue the cache evaluation. Do not call tools. Reply with one short sentence saying whether alpha and beta are both retained." \
  ""

run_orca \
  turn5-trigger-second-compact \
  continue \
  "Continue the cache evaluation. Do not call tools. Reply with one short sentence: gamma received." \
  "$EVAL_ROOT/blob3.txt"

run_orca \
  turn6-after-second-compact \
  continue \
  "Continue the cache evaluation. Do not call tools. Reply with one short sentence naming all retained clusters." \
  ""

python3 - "$EVAL_ROOT" <<'PY'
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
print(f"EVAL_ROOT={root}")
print(root.joinpath("exits.txt").read_text(), end="")

def parse_events(path):
    for line in path.read_text(errors="replace").splitlines():
        if not line.strip():
            continue
        try:
            yield json.loads(line)
        except json.JSONDecodeError:
            continue

print("\nMain calls:")
for path in sorted(root.glob("turn*.jsonl")):
    usages = []
    completed = None
    tools = []
    approvals = []
    errors = []
    for event in parse_events(path):
        typ = event.get("type")
        payload = event.get("payload") or {}
        if typ == "usage.updated":
            usages.append(payload)
        elif typ == "session.completed":
            completed = payload
        elif typ == "tool.call.requested":
            tools.append(payload.get("name"))
        elif typ == "approval.requested":
            approvals.append(payload)
        elif typ == "error":
            errors.append(payload)

    usage = usages[-1] if usages else None
    if usage:
        input_tokens = usage.get("input_tokens", 0)
        cache_tokens = usage.get("cache_tokens", 0)
        hit = (cache_tokens / input_tokens * 100) if input_tokens else 0.0
        print(
            f"{path.stem}: input={input_tokens} cache={cache_tokens} "
            f"hit={hit:.1f}% output={usage.get('output_tokens', 0)} "
            f"status={completed} tools={tools} approvals={len(approvals)} errors={errors}"
        )
    else:
        print(
            f"{path.stem}: NO_USAGE status={completed} "
            f"tools={tools} approvals={len(approvals)} errors={errors}"
        )

print("\nRemote summary telemetry:")
for path in sorted(root.glob("turn*.stderr")):
    lines = [
        line
        for line in path.read_text(errors="replace").splitlines()
        if line.startswith("orca.remote_summary")
    ]
    if not lines:
        continue
    print(path.stem + ":")
    for line in lines:
        print("  " + line)

print("\nTranscript records:")
for session in sorted((root / "orca" / "sessions").rglob("*.jsonl")):
    counts = {}
    special = []
    for event in parse_events(session):
        typ = event.get("type")
        counts[typ] = counts.get(typ, 0) + 1
        if typ in ("context.collapsed", "context.summary", "session.usage"):
            special.append(event)
    print(f"{session.name} {counts}")
    for event in special:
        print("  " + json.dumps(event, ensure_ascii=False))
PY

if [[ "$ORCA_EVAL_KEEP_DIR" != "1" ]]; then
  rm -rf "$EVAL_ROOT"
fi
