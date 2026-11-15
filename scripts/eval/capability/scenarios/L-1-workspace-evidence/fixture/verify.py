import json, os, pathlib, sys

OUT = pathlib.Path(os.environ["CAPABILITY_AGENT_STDOUT"])
LOG = pathlib.Path(os.environ["CAPABILITY_PROVIDER_LOG"])

def completed():
    for line in OUT.read_text(encoding="utf-8", errors="replace").splitlines():
        if '"type":"session.completed"' in line:
            try:
                return json.loads(line)["payload"]
            except Exception:
                continue
    return {}

def requests():
    if not LOG.exists():
        return []
    return [json.loads(line) for line in LOG.read_text(encoding="utf-8").splitlines() if line.strip()]

def fail(message):
    print("FAIL:", message)
    sys.exit(1)

payload = completed()
if payload.get("status") != "success":
    fail(f"run did not succeed: {payload.get('status')}")
observation = payload.get("workspace_observation") or {}
if observation.get("complete") is not True or observation.get("changed") is not True:
    fail(f"workspace observation missing changes: {observation}")
paths = {change.get("path") for change in observation.get("changes") or []}
if not {"file.txt", "new.txt"} <= paths:
    fail(f"changes do not mention both files: {sorted(paths)}")
if payload.get("writes_applied") is not False:
    fail("shell writes must not be reported as tool writes")
if payload.get("resumable") is not True or not (payload.get("resume") or {}).get("command"):
    fail("a recorded-history session must advertise resume evidence")
proof = (payload.get("completion_proof") or {}).get("verification") or {}
if proof.get("state") != "unverified":
    fail(f"shell-only work must stay unverified: {proof}")
print("ok: workspace/complete/resume evidence present")
