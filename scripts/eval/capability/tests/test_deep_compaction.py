"""C-real-deep-compaction: prepare and verifier, driven offline (#172).

A fake agent stands in for the runtime: it reads the seeded session, emits the
events a real run would, appends the compaction snapshot and writes the
answer. Each variant pins one thing the verifier must accept or refuse.
"""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

HERE = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(HERE))
from lib import backend, scenario

SCENARIO = HERE / "scenarios" / "C-real-deep-compaction" / "scenario.json"

FAKE_AGENT = r'''#!/usr/bin/env python3
import json, os, sys
from pathlib import Path

MODE = %(mode)r
if "trust" in sys.argv:
    raise SystemExit(0)
args = sys.argv
if "--resume" not in args:
    print("fake agent: no --resume", file=sys.stderr); raise SystemExit(3)
session_id = args[args.index("--resume") + 1]
matches = list(Path(os.environ["ORCA_HOME"], "sessions").rglob("*%%s.jsonl" %% session_id))
if len(matches) != 1:
    print("fake agent: seeded session not found", file=sys.stderr); raise SystemExit(4)
path = matches[0]
records = [json.loads(line) for line in path.read_text().splitlines()]
messages = [r["message"] for r in records if r.get("type") == "conversation.message"]
replies = [m["content"] for m in messages if m["role"] == "assistant"]
# What a faithful summarizer keeps: the sentence that names the code.
fact = next(s.strip() for reply in replies for s in reply.split(". ") if "deployment code" in s)
code = fact.split()[-1].strip(".;,")

def emit(kind, payload):
    print(json.dumps({"type": kind, "payload": payload}), flush=True)

if MODE == "transcript":
    emit("tool.call.requested", {"id": "t0", "name": "bash", "target": None,
                                  "raw_arguments": json.dumps({"command": "cat " + str(path)})})
    emit("tool.call.completed", {"id": "t0", "status": "completed", "output": path.read_text()})


def write_answer():
    emit("tool.call.requested", {"id": "t1", "name": "write_file", "target": "answer.txt",
                                  "raw_arguments": json.dumps({"path": "answer.txt", "content": code})})
    Path("answer.txt").write_text(code + "\n")
    emit("tool.call.completed", {"id": "t1", "status": "completed", "output": "Wrote answer.txt"})


if MODE == "early":
    write_answer()
strategy = "none" if MODE == "none" else "remote_summary"
kept = [m for m in messages if m["role"] != "assistant"] + [{"role": "user", "content": args[-1], "pinned": True}]
emit("context.compacted", {
    "reason": "exceeded_context_limit", "strategy": strategy,
    "before_messages": len(messages) + 1, "after_messages": len(messages) + 1 if MODE == "none" else len(kept),
    "collapsed_messages": 0 if MODE == "none" else len(replies),
    "observation": {"before_tokens_est": 16700, "after_tokens_est": 16700 if MODE == "none" else 6600,
                    "summary_requests": [] if MODE == "none" else [
                        {"purpose": "delta", "provider": "deepseek", "model": "deepseek-flash",
                         "outcome": "succeeded", "summary_cache_hit": False,
                         "usage": {"prompt_tokens": 4000, "completion_tokens": 300}}]}})
if MODE != "none":
    summary = "Reviewed the release checklist." if MODE == "lost" else "Reviewed the release checklist. " + fact + "."
    with path.open("a") as out:
        out.write(json.dumps({"type": "context.manual_compaction_snapshot", "strategy": "remote_summary",
                              "rolling_summary": summary, "messages": kept}) + "\n")
if MODE == "grep":
    # Names the code but not answer.txt, and reaches the transcript via "..".
    found = "\n".join(line for line in path.read_text().splitlines() if code in line)
    emit("tool.call.requested", {"id": "t2", "name": "bash", "target": None,
                                  "raw_arguments": json.dumps({"command": "grep -R " + code + " .."})})
    emit("tool.call.completed", {"id": "t2", "status": "completed", "output": found})
if MODE in ("combined", "combined_h"):
    # One call that searches the transcript and writes the answer. grep -R
    # prefixes each match with the file it came from; grep -rh does not, but
    # still prints the raw transcript record.
    relative = os.path.relpath(path, Path.cwd()) + ":" if MODE == "combined" else ""
    found = "\n".join(relative + line for line in path.read_text().splitlines() if code in line)
    search = "grep -R " + code + " .." if MODE == "combined" else "grep -rh deployment .."
    command = search + "; printf %%s " + code + " > answer.txt"
    emit("tool.call.requested", {"id": "t3", "name": "bash", "target": None,
                                  "raw_arguments": json.dumps({"command": command})})
    Path("answer.txt").write_text(code)
    emit("tool.call.completed", {"id": "t3", "status": "completed", "output": found})
if MODE == "split":
    # The code arrives split across two streamed chunks; the completion is truncated.
    text = next(line for line in path.read_text().splitlines() if code in line)
    cut = text.index(code) + len(code) // 2
    emit("tool.call.requested", {"id": "t4", "name": "bash", "target": None,
                                  "raw_arguments": json.dumps({"command": "grep -rh deployment .."})})
    emit("tool.output.delta", {"id": "t4", "chunk": text[:cut]})
    emit("tool.output.delta", {"id": "t4", "chunk": text[cut:]})
    emit("tool.call.completed", {"id": "t4", "status": "completed", "output": "", "truncated": True})
if MODE not in ("early", "combined", "combined_h"):
    write_answer()
emit("session.completed", {"status": "success"})
'''


class DeepCompactionRungTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.item = scenario.load(SCENARIO)

    def attempt(self, mode):
        binary = self.root / f"fake-orca-{mode}"
        binary.write_text(FAKE_AGENT % {"mode": mode})
        binary.chmod(0o755)
        with patch.object(backend, "MockProvider") as mock:
            mock.return_value.port = 12345
            result = backend.run_local(self.item, self.root / mode, binary, provider="mock")
        log = (self.root / mode / "verify-0.log")
        return result, log.read_text() if log.exists() else ""

    def test_a_summary_that_kept_the_code_passes(self):
        result, log = self.attempt("kept")
        self.assertEqual(result.final_verification, "passed", log)
        self.assertEqual(result.outcome, "pass", log)

    def test_a_summary_that_lost_the_code_fails(self):
        result, log = self.attempt("lost")
        self.assertEqual(result.final_verification, "failed", log)
        self.assertIn("summary", log)

    def test_a_compaction_that_shrank_nothing_fails(self):
        result, log = self.attempt("none")
        self.assertEqual(result.final_verification, "failed", log)
        self.assertIn("remote_summary", log)

    def test_reading_the_saved_transcript_fails(self):
        result, log = self.attempt("transcript")
        self.assertEqual(result.final_verification, "failed", log)
        self.assertIn("transcript", log)

    def test_a_search_that_names_the_code_still_counts_as_a_disk_read(self):
        # Only the answer write proves the agent already had the code; a
        # search for it must not switch off the tool-output check.
        result, log = self.attempt("grep")
        self.assertEqual(result.final_verification, "failed", log)
        self.assertIn("tool output", log)

    def test_an_answer_written_before_the_summary_fails(self):
        result, log = self.attempt("early")
        self.assertEqual(result.final_verification, "failed", log)
        self.assertIn("before", log)

    def test_a_call_that_reads_the_transcript_and_writes_the_answer_fails(self):
        # The output shows the transcript's file name, whatever the call looks like.
        result, log = self.attempt("combined")
        self.assertEqual(result.final_verification, "failed", log)
        self.assertIn("transcript", log)

    def test_a_filename_less_transcript_read_in_the_answer_call_fails(self):
        # grep -rh hides the file name but prints the raw transcript record.
        result, log = self.attempt("combined_h")
        self.assertEqual(result.final_verification, "failed", log)
        self.assertIn("transcript", log)

    def test_a_code_split_across_output_chunks_is_still_caught(self):
        result, log = self.attempt("split")
        self.assertEqual(result.final_verification, "failed", log)
        self.assertIn("tool output", log)

    def test_the_seeded_session_hides_the_code_in_one_old_unpinned_reply(self):
        home, workspace = self.root / "home", self.root / "workspace"
        home.mkdir()
        workspace.mkdir()
        prepared = subprocess.run([sys.executable, "-B", str(self.item.prepare)], cwd=workspace,
                                  env={**os.environ, "CAPABILITY_HOME": str(home)}, capture_output=True, text=True)
        self.assertEqual(prepared.returncode, 0, prepared.stderr)
        sessions = list((home / "sessions").rglob("*.jsonl"))
        self.assertEqual(len(sessions), 1)
        records = [json.loads(line) for line in sessions[0].read_text().splitlines()]
        self.assertEqual(records[0]["type"], "session.meta")
        resume = self.item.exec_flags
        self.assertEqual(resume[:1], ["--resume"])
        self.assertEqual(records[0]["session_id"], resume[1])
        messages = [r["message"] for r in records[1:]]
        users = [m for m in messages if m["role"] == "user"]
        replies = [m for m in messages if m["role"] == "assistant"]
        self.assertGreaterEqual(len(replies), 6)
        self.assertTrue(all(m["pinned"] for m in users))
        self.assertFalse(any(m["pinned"] for m in replies))
        holders = [index for index, m in enumerate(replies) if "deployment code" in m["content"]]
        self.assertEqual(holders, [0], "only the oldest reply may name the code")
        self.assertFalse(any("ORCA-" in m["content"] for m in users))
        # Comfortably past the scenario's 16k auto-compaction line (about 4 chars per token).
        self.assertGreater(sum(len(m["content"]) for m in replies) // 4, 16000)


if __name__ == "__main__":
    unittest.main()
