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

if calls("read_document") < 1:
    fail("read_document was not called")
answer = pathlib.Path("ocr.txt")
if not answer.exists():
    fail("ocr.txt is missing")
text = answer.read_text(encoding="utf-8")
if "ORCA-OCR-MARKER-2026" not in text:
    fail(f"recognized marker missing: {text[:200]!r}")
cache = HOME / "ocr_cache"
entries = [p for p in cache.rglob("*") if p.is_file()] if cache.exists() else []
if not entries:
    fail("no OCR cache entry was written under $ORCA_HOME/ocr_cache")
print(f"ok: OCR returned the marker and cached {len(entries)} entry(ies)")
