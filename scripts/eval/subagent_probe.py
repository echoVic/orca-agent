#!/usr/bin/env python3
"""Subagent / task-control probes (v0.4.31 unified capacity + task control plane).

Drives `orca exec` against scripts/eval/mock_provider.py in `subagent` mode: the
parent is told to delegate (optionally with a role, schema or deadline), the
child conversations are served by the same provider, and the parent must collect
the result through the task tools. This is the L1 surface for delegation, which
Terminal-Bench barely exercises.

Usage:
    python3 scripts/eval/subagent_probe.py [--binary target/release/orca]
"""

from __future__ import annotations

import argparse
import json
import os
import shlex
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import mock_provider  # noqa: E402

CASES = [
    {
        "name": "spawn_and_collect",
        "tokens": "FIMODE=tool_script FITOOL=subagent",
        "expect_status": "success",
        "expect_child": True,
        "note": "parent delegates, waits, and reports; child conversation runs",
    },
    {
        "name": "explorer_role",
        "tokens": "FIMODE=tool_script FITOOL=subagent FITYPE=explorer",
        "expect_status": "success",
        "expect_child": True,
        "note": "read-only role child runs to completion",
    },
    {
        "name": "typed_schema_child",
        "tokens": "FIMODE=tool_script FITOOL=subagent FISCHEMA=1",
        "expect_status": "success",
        "expect_child": True,
        "note": "child output validated against a schema",
    },
    {
        "name": "parallel_batch_three",
        "tokens": "FIMODE=tool_script FITOOL=subagent FIBATCH=3",
        "expect_status": "success",
        "expect_child": True,
        "expect_children_min": 3,
        "timeout": 90,
        "note": "three children accepted in one turn, all collected",
    },
    {
        "name": "child_deadline",
        "tokens": "FIMODE=tool_script FITOOL=subagent FIDEADLINE=1",
        "expect_status_any": ["success", "failed"],
        "expect_child": None,
        "max_wall_s": 90,
        "timeout": 60,
        "note": "a 1 ms child deadline must terminate truthfully, never hang the parent",
    },
]


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def run_case(binary: str, port: int, case: dict, timeout: float, out_dir: Path) -> dict:
    prompt = case["tokens"]
    with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as cwd:
        command = (
            f"orca exec --mode full-auto --output-format jsonl"
            f" --base-url http://127.0.0.1:{port} --api-key subagent-probe"
            f" --cwd {shlex.quote(cwd)} --no-history -- {shlex.quote(prompt)}"
        )
        env = dict(os.environ)
        env["ORCA_HOME"] = home
        env["PATH"] = f"{Path(binary).parent}{os.pathsep}{env.get('PATH','')}"
        env.pop("DEEPSEEK_API_KEY", None)
        started = time.time()
        try:
            completed = subprocess.run(
                ["/bin/sh", "-c", command],
                capture_output=True,
                text=True,
                timeout=timeout,
                env=env,
            )
            stdout, code = completed.stdout, completed.returncode
        except subprocess.TimeoutExpired:
            stdout, code = "", None
    (out_dir / f"{case['name']}.jsonl").write_text(stdout, encoding="utf-8")

    status = None
    subagent_results: list[dict] = []
    wait_reasons: list[str] = []
    errors: list[str] = []
    for line in stdout.splitlines():
        if not line.startswith("{"):
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        kind = event.get("type")
        payload = event.get("payload") or {}
        if kind == "session.completed":
            status = payload.get("status")
        elif kind == "tool.call.completed":
            name = payload.get("name")
            raw = payload.get("output") or payload.get("error")
            detail = {}
            if isinstance(raw, str) and raw.strip().startswith("{"):
                try:
                    detail = json.loads(raw)
                except json.JSONDecodeError:
                    detail = {}
            if name == "subagent":
                subagent_results.append(detail)
            elif name == "task_wait":
                wait_reasons.append(str(detail.get("return_reason")))
        elif kind == "task.status.updated":
            error = ((payload.get("task") or {}).get("error")) or None
            if error:
                errors.append(error[:160])
    return {
        "exit_code": code,
        "status": status,
        "wall_s": round(time.time() - started, 1),
        "subagent_results": subagent_results,
        "wait_reasons": wait_reasons,
        "errors": errors,
    }


def verdict(case: dict, result: dict, child_requests: int) -> tuple[str, list[str]]:
    notes: list[str] = []
    ok = True
    if result["exit_code"] != 0:
        ok = False
        notes.append(f"exit={result['exit_code']}")
    expected = case.get("expect_status_any") or [case["expect_status"]]
    if result["status"] not in expected:
        ok = False
        notes.append(f"session status={result['status']} expected {expected}")
    if case.get("expect_children_min") and child_requests < case["expect_children_min"]:
        ok = False
        notes.append(f"only {child_requests} child conversations ran")
    if case.get("expect_child") and child_requests == 0:
        ok = False
        notes.append("child conversation never ran")
    if case.get("expect_child") is False and child_requests:
        ok = False
        notes.append(f"child ran {child_requests}× despite expectation")
    if case.get("max_wall_s") and result["wall_s"] > case["max_wall_s"]:
        ok = False
        notes.append(f"wall {result['wall_s']}s > {case['max_wall_s']}s (possible hang)")
    if result["errors"]:
        ok = False
        notes.append(f"task error: {result['errors'][0]}")
    return ("PASS" if ok else "FAIL"), notes


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/release/orca")
    parser.add_argument("--out", default="jobs/eval-subagent-probe")
    parser.add_argument("--timeout", type=float, default=180.0)
    args = parser.parse_args()

    binary = str(Path(args.binary).resolve())
    if not Path(binary).exists():
        print(f"binary not found: {binary}", file=sys.stderr)
        return 2

    stamp = time.strftime("%Y%m%d-%H%M%S")
    out_dir = Path(args.out) / stamp
    out_dir.mkdir(parents=True, exist_ok=True)
    request_log = out_dir / "requests.jsonl"
    port = free_port()
    server = mock_provider.serve(port, str(request_log))
    results = []
    try:
        for case in CASES:
            before = (
                len(request_log.read_text(encoding="utf-8").splitlines())
                if request_log.exists()
                else 0
            )
            result = run_case(binary, port, case, case.get("timeout", args.timeout), out_dir)
            entries = [
                json.loads(line)
                for line in request_log.read_text(encoding="utf-8").splitlines()
                if line.strip()
            ][before:] if request_log.exists() else []
            child_requests = len([e for e in entries if "READY" in (e.get("prompt") or "")])
            status, notes = verdict(case, result, child_requests)
            results.append(
                {
                    "case": case["name"],
                    "note": case["note"],
                    "verdict": status,
                    "notes": notes,
                    "child_requests": child_requests,
                    "result": result,
                }
            )
            waits = ",".join(result["wait_reasons"]) or "-"
            print(
                f"[{status}] {case['name']:<20} status={result['status']!s:<8} "
                f"exit={result['exit_code']!s:<4} children={child_requests} waits={waits} "
                f"wall={result['wall_s']}s {'; '.join(notes)}",
                flush=True,
            )
    finally:
        server.shutdown()

    report = {
        "binary": binary,
        "started_at": stamp,
        "cases": results,
        "summary": {
            "pass": len([r for r in results if r["verdict"] == "PASS"]),
            "fail": len([r for r in results if r["verdict"] == "FAIL"]),
        },
    }
    (out_dir / "report.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(f"\nreport: {out_dir / 'report.json'}")
    return 1 if report["summary"]["fail"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
