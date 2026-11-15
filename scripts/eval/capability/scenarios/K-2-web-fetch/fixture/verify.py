import json, os, pathlib, sys

OUT = pathlib.Path(os.environ["CAPABILITY_AGENT_STDOUT"])
HOME = pathlib.Path(os.environ["CAPABILITY_HOME"])

def events_text():
    return OUT.read_text(encoding="utf-8", errors="replace") if OUT.exists() else ""

def calls(tool):
    total = 0
    for line in events_text().splitlines():
        if '"type":"tool.call.requested"' not in line:
            continue
        try:
            data = json.loads(line)
        except json.JSONDecodeError:
            continue
        if (data.get("payload") or {}).get("name") == tool:
            total += 1
    return total

def fail(message):
    print("FAIL:", message)
    sys.exit(1)

if calls("web_fetch") < 1:
    fail("web_fetch was not called")
answer = pathlib.Path("title.txt")
if not answer.exists():
    fail("title.txt is missing")
text = answer.read_text(encoding="utf-8")
if "Example Domain" not in text:
    fail(f"fetched page title missing: {text[:200]!r}")
print("ok: web_fetch returned the page title")
