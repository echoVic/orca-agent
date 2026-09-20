#!/usr/bin/env python3
"""Task-control recovery probe: what a *second* process can see.

`subagent_probe.py` covers delegation inside one process (submit → `task_wait` →
result). This suite covers the boundary the docs leave implicit
(`docs/subagents.md`: "`subagent` has one submission protocol and no model-facing
`mode` field … use `task_wait`, `task_read_output`, `task_list`, `task_stop`"):

* a delegated task is accepted immediately with a `task_id` and settles through
  the task tools, with a durable reservation under `$ORCA_HOME/task-sessions/`;
* the same task id, used from a **fresh process** against the same `ORCA_HOME`,
  is rejected explicitly ("not started in this session … or belongs to another
  owner") instead of hanging, silently succeeding, or crashing.

The second half is a contract lock, not a bug hunt: if orca ever grows
cross-process task reads, this suite is where that change becomes visible.

Usage:
    python3 scripts/eval/task_recovery_probe.py [--binary target/release/orca]
"""

from __future__ import annotations

import argparse
import json
import os
import socket
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import mock_provider  # noqa: E402


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/release/orca")
    parser.add_argument("--out", default="jobs/eval-task-recovery-probe")
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    if not Path(binary).exists():
        print(f"binary not found: {binary}", file=sys.stderr)
        return 2

    stamp = __import__("time").strftime("%Y%m%d-%H%M%S")
    out_dir = Path(args.out) / stamp
    out_dir.mkdir(parents=True, exist_ok=True)
    port = free_port()
    provider = mock_provider.serve(port, str(out_dir / "provider-requests.jsonl"), "127.0.0.1")
    home = tempfile.mkdtemp(prefix="orca-task-recovery-home-")
    work = tempfile.mkdtemp(prefix="orca-task-recovery-work-")
    env = dict(os.environ)
    env.update({
        "ORCA_HOME": home,
        "ORCA_API_KEY": "task-recovery-probe",
        "ORCA_BASE_URL": f"http://127.0.0.1:{port}",
    })
    env.pop("DEEPSEEK_API_KEY", None)

    checks: list[tuple[str, bool, str, str]] = []
    results: list[dict] = []

    def record(name: str, ok: bool, detail: str, issue: str = "") -> None:
        checks.append((name, ok, detail, issue))
        results.append({"case": name, "ok": ok, "detail": detail, "issue": issue})

    def run(prompt: str, label: str, timeout: float = 300.0) -> list[dict]:
        completed = subprocess.run(
            [binary, "exec", "--mode", "full-auto", "--output-format", "jsonl",
             "--cwd", work, "--", prompt],
            capture_output=True, text=True, env=env, cwd=work, timeout=timeout,
        )
        (out_dir / f"{label}.jsonl").write_text(completed.stdout, encoding="utf-8")
        events = []
        for line in completed.stdout.splitlines():
            if line.strip().startswith("{"):
                try:
                    events.append(json.loads(line))
                except json.JSONDecodeError:
                    continue
        return events

    def tool_results(events: list[dict], name: str) -> list[dict]:
        out = []
        for event in events:
            if event.get("type") == "tool.call.completed" and (event.get("payload") or {}).get("name") == name:
                payload = event["payload"]
                raw = payload.get("output") or payload.get("error") or ""
                detail: dict | str = raw
                if isinstance(raw, str) and raw.strip().startswith("{"):
                    try:
                        detail = json.loads(raw)
                    except json.JSONDecodeError:
                        detail = raw
                out.append({"detail": detail, "status": payload.get("status"), "error": payload.get("error")})
        return out

    try:
        first = run("FIMODE=tool_script FITOOL=subagent", "run1-submit")
        submissions = tool_results(first, "subagent")
        task_id = None
        for entry in submissions:
            detail = entry["detail"]
            if isinstance(detail, dict):
                task_id = detail.get("task_id") or detail.get("agent_id")
                if task_id:
                    break
        record(
            "subagent accepted with a task_id",
            bool(task_id),
            f"submissions={len(submissions)} task_id={task_id}",
        )
        waits = tool_results(first, "task_wait")
        reasons = [str(entry["detail"].get("return_reason")) for entry in waits if isinstance(entry["detail"], dict)]
        record(
            "task_wait settles the delegated task",
            any(reason.startswith("target_reached") or reason.startswith("timeout") for reason in reasons),
            f"return_reasons={reasons}",
        )
        task_dirs = [path for path in Path(home, "task-sessions").glob("*") if path.is_dir()] if Path(home, "task-sessions").exists() else []
        record(
            "durable task reservation on disk",
            bool(task_dirs),
            f"task-sessions={[path.name for path in task_dirs][:3]}",
        )

        if task_id:
            second = run(f"FIMODE=tool_script FITOOL=taskresume FITASK={task_id}", "run2-foreign")
            reads = tool_results(second, "task_read_output")
            text = json.dumps(reads)
            explicit = bool(reads) and any(entry["status"] == "failed" for entry in reads) and "unknown task" in text
            record(
                "foreign task_read_output rejected explicitly",
                explicit,
                f"results={text[:150]}",
            )
            waits2 = tool_results(second, "task_wait")
            text2 = json.dumps(waits2)
            settled = bool(waits2) and any(entry["status"] in ("success", "failed") for entry in waits2)
            record(
                "foreign task_wait returns without hanging",
                settled,
                f"results={text2[:150]}",
            )
    finally:
        provider.shutdown()

    (out_dir / "checks.jsonl").write_text("\n".join(json.dumps(item) for item in results) + "\n")
    failures = 0
    for name, ok, detail, issue in checks:
        failures += 0 if ok else 1
        suffix = f" — {issue}" if issue and not ok else ""
        print(f"[{'PASS' if ok else 'FAIL'}] {name:<42} {detail[:100]}{suffix}")
    print(f"\nlogs: {out_dir}")
    print(f"verdict: {'PASS' if not failures else f'FAIL ({failures} checks)'}")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
