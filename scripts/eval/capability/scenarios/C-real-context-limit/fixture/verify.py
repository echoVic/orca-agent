"""Oversized prompt must still run through the runtime's exhausted-context path and answer exactly."""
import json, os, pathlib, sys

OUT = pathlib.Path(os.environ["CAPABILITY_AGENT_STDOUT"])
NEEDLE = "ORCA-ZETA-7742"

def fail(message):
    print("FAIL:", message)
    sys.exit(1)

events = []
for line in OUT.read_text(encoding="utf-8", errors="replace").splitlines():
    try:
        events.append(json.loads(line))
    except json.JSONDecodeError:
        continue
compactions = [e for e in events if e.get("type") == "context.compacted"]
if not compactions:
    fail("the runtime never emitted a context.compacted event for an oversized prompt")
payloads = [event.get("payload") or {} for event in compactions]
if not any(p.get("reason") == "exceeded_context_limit" for p in payloads):
    fail("unexpected compaction reasons: %r" % ([p.get("reason") for p in payloads],))
observations = [p.get("observation") or {} for p in payloads if p.get("observation")]
if not observations or observations[-1].get("before_tokens_est") in (None, 0):
    fail("compaction events carry no token observation")
answer = pathlib.Path("answer.txt")
if not answer.exists():
    fail("answer.txt is missing")
text = answer.read_text(encoding="utf-8").strip()
if text != NEEDLE:
    fail("wrong answer after compaction: %r" % text[:200])
completed = [e for e in events if e.get("type") == "session.completed"]
if not completed:
    fail("session.completed missing")
status = (completed[-1].get("payload") or {}).get("status")
if status != "success":
    fail("run did not succeed: %r" % (status,))
print("ok: %d compaction event(s), answer preserved" % len(compactions))
