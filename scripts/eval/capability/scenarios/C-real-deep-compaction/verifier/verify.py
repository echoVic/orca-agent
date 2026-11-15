"""C-real-deep-compaction: a fact from an old assistant reply must survive a real summary.

The resumed session names the deployment code only in turn 1's assistant reply.
User messages are pinned and kept verbatim, but that reply is not, so the code
reaches the answer only if the runtime summarized the old turns (strategy
remote_summary) and the summary kept it. Checked here:

1. the measured run's compaction was a real deep one: it collapsed messages,
   cut the token estimate, and its summary requests succeeded with usage;
2. the snapshot saved in the session holds the code in the summary, and no
   kept message holds it verbatim;
3. no tool call read the saved transcript, and the code reached no tool
   output before the agent wrote it;
4. answer.txt is exactly the code and the run succeeded.
"""

import json
import os
import pathlib
import sys

CODE = "ORCA-KESTREL-4471"
SESSION_ID = "5e5c0172-0000-4000-8000-00000000c172"


def fail(message):
    print("FAIL:", message)
    sys.exit(1)


def jsonl(path):
    records = []
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict):
            records.append(value)
    return records


events = jsonl(pathlib.Path(os.environ["CAPABILITY_AGENT_STDOUT"]))
home = os.environ["CAPABILITY_HOME"]

# 1. A real deep compaction, not a no-shrink attempt.
compactions = [event.get("payload") or {} for event in events if event.get("type") == "context.compacted"]
deep = [payload for payload in compactions if payload.get("strategy") == "remote_summary"]
if not deep:
    fail("no remote_summary compaction; strategies seen: %r" % [p.get("strategy") for p in compactions])
compaction = deep[0]
collapsed = compaction.get("collapsed_messages") or 0
if collapsed <= 0:
    fail("the compaction collapsed no messages")
before_messages, after_messages = compaction.get("before_messages") or 0, compaction.get("after_messages") or 0
if before_messages <= after_messages:
    fail("message count did not drop: %r -> %r" % (before_messages, after_messages))
observation = compaction.get("observation") or {}
before_tokens, after_tokens = observation.get("before_tokens_est") or 0, observation.get("after_tokens_est") or 0
if before_tokens <= after_tokens:
    fail("token estimate did not drop: %r -> %r" % (before_tokens, after_tokens))
requests = observation.get("summary_requests") or []
if not requests:
    fail("the compaction made no summary request")
if any(request.get("outcome") != "succeeded" for request in requests):
    fail("a summary request did not succeed: %r" % [request.get("outcome") for request in requests])
if any(not request.get("summary_cache_hit") and not request.get("usage") for request in requests):
    fail("a summary request carries no usage")

# 2. The saved summary kept the code; the reply that held it is gone.
sessions = list(pathlib.Path(home, "sessions").rglob("*%s.jsonl" % SESSION_ID))
if len(sessions) != 1:
    fail("resumed session not found (%d matches)" % len(sessions))
snapshots = [record for record in jsonl(sessions[0])
             if record.get("type") == "context.manual_compaction_snapshot"
             and record.get("strategy") == "remote_summary"]
if not snapshots:
    fail("the session saved no remote_summary compaction snapshot")
snapshot = snapshots[0]
if CODE not in str(snapshot.get("rolling_summary") or ""):
    fail("the compaction summary lost the deployment code")
if any(CODE in json.dumps(message) for message in snapshot.get("messages") or []):
    fail("a kept message still holds the code verbatim, so the summary was not what carried it")

# 3. Recalled from the conversation, not read back from disk.
wrote = False
for event in events:
    kind, payload = event.get("type"), event.get("payload") or {}
    if kind == "tool.call.requested":
        call = json.dumps([payload.get("target"), payload.get("raw_arguments")])
        if home in call or SESSION_ID in call or "/sessions/" in call:
            fail("a tool call read the saved transcript: %s" % call[:200])
        if CODE in call:
            wrote = True
    elif kind in ("tool.call.completed", "tool.output.delta") and not wrote and CODE in json.dumps(payload):
        fail("the code showed up in a tool output before the agent wrote it, so it came from disk")

# 4. The answer and the run.
answer = pathlib.Path("answer.txt")
if not answer.exists():
    fail("answer.txt is missing")
text = answer.read_text(encoding="utf-8").strip()
if text != CODE:
    fail("wrong answer after deep compaction: %r" % text[:200])
completed = [event for event in events if event.get("type") == "session.completed"]
if not completed:
    fail("session.completed missing")
status = (completed[-1].get("payload") or {}).get("status")
if status != "success":
    fail("run did not succeed: %r" % (status,))
print("ok: remote_summary collapsed %d message(s), %d -> %d tokens est, %d summary request(s); the code survived"
      % (collapsed, before_tokens, after_tokens, len(requests)))
