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
status = payload.get("status")
blockers = payload.get("blockers") or []
if status != "blocked":
    fail(f"blocker report should end blocked, got {status!r}: {payload.get('terminal')}")
if not blockers:
    fail("blockers list is empty")
kinds = {b.get("kind") for b in blockers}
if "user_decision" not in kinds:
    fail(f"unexpected blocker kinds: {kinds}")
if payload.get("writes_applied") is not False:
    fail("no writes were applied")
print("ok: blocked status and blocker evidence recorded")
