import json, os, pathlib, subprocess, sys

HOME = os.environ["CAPABILITY_HOME"]
BIN = os.environ["CAPABILITY_ORCA_BIN"]
WORKSPACE = pathlib.Path.cwd()

def run_rewind(*args):
    env = {**os.environ, "ORCA_HOME": HOME}
    return subprocess.run([BIN, "rewind", *args], capture_output=True, text=True, timeout=120, env=env, cwd=WORKSPACE)

def session_records():
    records = []
    for path in pathlib.Path(HOME, "sessions").rglob("*.jsonl"):
        for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
            try:
                records.append(json.loads(line))
            except json.JSONDecodeError:
                continue
    return records

def session_rewinds():
    out = []
    for record in session_records():
        data = record.get("payload") or {}
        if record.get("type") == "session.rewind" or data.get("type") == "session.rewind":
            out.append(record)
    return out

def fail(message):
    print("FAIL:", message)
    sys.exit(1)

prune = run_rewind("--prune", "--json")
if prune.returncode != 0:
    fail(f"rewind --prune exited {prune.returncode}: {prune.stderr[-300:]}")
rollback = run_rewind("--rollback", "rw_does_not_exist")
if rollback.returncode == 0:
    fail("rewind --rollback accepted an unknown operation id")
combined = rollback.stdout + rollback.stderr
if "panic" in combined.lower():
    fail("rewind --rollback panicked instead of failing cleanly")
empty_home = pathlib.Path(HOME, "no-sessions-here")
os.environ["ORCA_HOME"] = str(empty_home)
env = {**os.environ, "ORCA_HOME": str(empty_home)}
missing = subprocess.run([BIN, "rewind", "--last"], capture_output=True, text=True, timeout=60, env=env, cwd=WORKSPACE)
if missing.returncode == 0:
    fail("rewind --last succeeded with no recorded sessions")
print("ok: prune/rollback/empty-home contracts hold")
