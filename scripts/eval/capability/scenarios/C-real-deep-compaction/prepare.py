"""Seed the attempt's ORCA_HOME with the session C-real-deep-compaction resumes.

Eight earlier turns of a release checklist review. Each user ask is short:
user messages are saved pinned, and compaction keeps pinned messages verbatim.
Each assistant reply is a long note, unpinned, so compaction must collapse the
old ones into a summary. The deployment code appears once, in the first
reply, so the measured turn can only get it from that summary.

Run by the harness with the workspace as the working directory and
CAPABILITY_HOME set to the attempt's ORCA_HOME.
"""

import datetime
import json
import os
from pathlib import Path

SESSION_ID = "5e5c0172-0000-4000-8000-00000000c172"
CODE = "ORCA-KESTREL-4471"
REPLY_CHARS = 9_000

SECTIONS = [
    ("release header and scope", ["scope statement", "release owner", "freeze window", "change ticket",
                                  "version string", "affected services", "customer notice", "approval list"]),
    ("database migrations", ["schema change", "backfill job", "index build", "lock timeout",
                             "replica lag budget", "migration dry run", "down migration", "row count check"]),
    ("feature flags", ["flag inventory", "default values", "kill switch", "percentage ramp",
                       "stale flag cleanup", "flag owner", "audit log", "override list"]),
    ("canary rollout", ["canary cohort", "traffic split", "error budget", "latency target",
                        "promotion gate", "soak time", "regional order", "abort criteria"]),
    ("observability and alerts", ["dashboard links", "alert thresholds", "paging policy", "log sampling",
                                  "trace retention", "synthetic checks", "SLO burn alert", "runbook links"]),
    ("rollback plan", ["rollback trigger", "artifact pinning", "data compatibility", "rollback rehearsal",
                       "config revert", "cache invalidation", "rollback owner", "customer impact note"]),
    ("documentation and support handoff", ["release notes draft", "support macro", "known issues list",
                                           "status page text", "internal FAQ", "escalation path",
                                           "training session", "handoff checklist"]),
    ("post-release verification", ["smoke test suite", "error rate review", "billing reconciliation",
                                   "usage dashboards", "follow-up tickets", "retro invite",
                                   "metric snapshot", "sign-off record"]),
]
CHECKS = ["re-read", "cross-checked", "walked through", "compared it with the previous release",
          "confirmed it with its owner", "traced it to its runbook", "checked it against the plan"]
STATES = ["it is unchanged from the previous release", "it is still accurate",
          "it was updated last week and is consistent", "it needs no action this time",
          "the platform team owns it and has signed off", "the existing runbook covers it",
          "nothing in it conflicts with the other sections"]
FOLLOW_UPS = ["No follow-up needed.", "I'll mention it in the summary.", "Worth a glance at sign-off.",
              "The owner has the details.", "It matches the ticket.", "Nothing blocks on it."]


def reply(index: int, title: str, subjects: list[str]) -> str:
    lines = [f"Notes on section {index + 1}, {title}."]
    if index == 0:
        lines.append(f"The checklist header fixes the deployment code for this release: {CODE}. "
                     "The final release step must pass that exact code, so keep it for the end of the review.")
    item = 0
    while sum(len(line) + 1 for line in lines) < REPLY_CHARS:
        subject = subjects[item % len(subjects)]
        check = CHECKS[(item + index) % len(CHECKS)]
        state = STATES[(item * 3 + index) % len(STATES)]
        follow_up = FOLLOW_UPS[(item + 2 * index) % len(FOLLOW_UPS)]
        lines.append(f"{index + 1}.{item + 1:02d} {subject} (pass {item // len(subjects) + 1}): "
                     f"I {check}; {state}. {follow_up}")
        item += 1
    return "\n".join(lines)


def main() -> None:
    home = Path(os.environ["CAPABILITY_HOME"])
    workspace = str(Path.cwd())
    now = datetime.datetime.now(datetime.timezone.utc)
    records = [{
        "type": "session.meta", "schema_version": 1, "session_id": SESSION_ID, "cwd": workspace,
        "provider": "deepseek", "model": "auto", "title": "release checklist review",
        "created_at": now.strftime("%Y-%m-%dT%H:%M:%S.%fZ"), "forked": False, "approval_mode": "full-auto",
        "runtime_workspace_roots": [workspace], "permission_rules": {"rules": []},
        "additional_working_directories": [],
    }]
    for index, (title, subjects) in enumerate(SECTIONS):
        ask = f"Let's go through section {index + 1} of the release checklist, {title}. Give me your notes."
        records.append({"type": "conversation.message",
                        "message": {"role": "user", "content": ask, "pinned": True}})
        records.append({"type": "conversation.message",
                        "message": {"role": "assistant", "content": reply(index, title, subjects),
                                    "reasoning_content": None, "tool_calls": [], "pinned": False}})
    day = home / "sessions" / now.strftime("%Y/%m/%d")
    day.mkdir(parents=True, exist_ok=True)
    path = day / f"session-{now.strftime('%Y-%m-%dT%H-%M-%S')}-{SESSION_ID}.jsonl"
    path.write_text("".join(json.dumps(record) + "\n" for record in records), encoding="utf-8")


if __name__ == "__main__":
    main()
