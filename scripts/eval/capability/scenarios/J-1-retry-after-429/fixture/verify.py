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
if len(rows) < 2:
    fail(f"expected a retried request, saw {len(rows)}")
gap = rows[1]["ts"] - rows[0]["ts"]
if gap < 0.9:
    fail(f"Retry-After was not honoured: gap {gap:.3f}s")
payload = completed()
if payload.get("status") != "success":
    fail(f"run did not recover: {payload.get('status')}")
done = pathlib.Path("done.txt")
if not done.exists() or done.read_text().strip() != "done":
    fail("the tool call did not run after the retry")
print(f"ok: 429 retried after {gap:.2f}s, task completed")
