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

rows = requests()
if len(rows) < 2:
    fail(f"expected retries before the failure, saw {len(rows)}")
payload = completed()
if payload.get("status") != "failed":
    fail(f"persistent failures should end failed: {payload.get('status')}")
if payload.get("resumable") is not True:
    fail("a saved-history failure should be resumable")
if not (payload.get("resume") or {}).get("command"):
    fail("resume.command missing on a resumable failure")
observation = payload.get("workspace_observation") or {}
if observation.get("complete") is not True:
    fail("workspace observation incomplete")
if observation.get("changed") is not False:
    fail(f"no tool ran, so nothing should change: {observation}")
if payload.get("writes_applied") is not False:
    fail("failed run must not report applied writes")
print("ok: failure evidence carries resume and workspace state")
