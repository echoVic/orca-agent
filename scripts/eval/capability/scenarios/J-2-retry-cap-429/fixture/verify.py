import json, os, pathlib, sys

LOG = pathlib.Path(os.environ["CAPABILITY_PROVIDER_LOG"])
OUT = pathlib.Path(os.environ["CAPABILITY_AGENT_STDOUT"])

def requests():
    if not LOG.exists():
        return []
    return [json.loads(line) for line in LOG.read_text(encoding="utf-8").splitlines() if line.strip()]

def completed():
    for line in OUT.read_text(encoding="utf-8", errors="replace").splitlines():
        if '"type":"session.completed"' in line:
            try:
                return json.loads(line)["payload"]
            except Exception:
                continue
    return {}

def fail(message):
    print("FAIL:", message)
    sys.exit(1)

rows = requests()
if not 2 <= len(rows) <= 6:
    fail(f"retry cap not respected: {len(rows)} requests")
payload = completed()
if payload.get("status") != "failed":
    fail(f"persistent 429 should fail the run: {payload.get('status')}")
message = json.dumps(payload)
if "max retries" not in message or "429" not in message:
    fail("failure did not explain the retry cap / status")
if payload.get("resumable") is not False:
    fail("no-history failure should not claim to be resumable")
print(f"ok: stopped after {len(rows)} requests with a clean provider error")
