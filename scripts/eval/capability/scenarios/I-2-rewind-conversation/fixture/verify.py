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

result = run_rewind("--last", "--mode", "conversation", "--json")
if result.returncode != 0:
    fail(f"rewind exited {result.returncode}: {result.stdout[-400:]} {result.stderr[-400:]}")
body = json.loads(result.stdout)
if body.get("mode") != "conversation":
    fail(f"unexpected mode: {body}")
kept = (body.get("conversation") or {}).get("kept_messages")
if kept is None or kept > 2:
    fail(f"conversation was not truncated: kept={kept}")
text = (WORKSPACE / "file.txt").read_text(encoding="utf-8")
if text != "changed by the rewind fixture\n":
    fail(f"conversation rewind should not touch files: {text!r}")
if not session_rewinds():
    fail("no session.rewind record was appended")
print("ok: conversation truncated, files untouched")
