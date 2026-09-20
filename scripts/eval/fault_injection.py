#!/usr/bin/env python3
"""Fault-injection suite for Orca's provider/runtime recovery paths.

Runs a set of scenarios against ``scripts/eval/mock_provider.py`` and reports
whether the recovery behaviour matches the documented contract. This is the L1
"工具与交互契约" probe from docs/evaluation-plan.md: cheap, deterministic, and
aimed at the failure classes that cost whole Terminal-Bench trials.

Usage:
    python3 scripts/eval/fault_injection.py [--binary target/release/orca]
                                            [--out jobs/eval-fault-injection]
"""

from __future__ import annotations

import argparse
import json
import os
import re
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import mock_provider  # noqa: E402  (local module next to this file)

TERMINAL_MARKER = "stream ended before terminal marker"

SCENARIOS = [
    {
        "name": "duplicate_tool_call_id",
        "tokens": "FIMODE=tool_script FITOOL=dup_tool_id",
        "expect_status": "success",
        "expect_attempts": None,
        "note": "a provider that reuses a tool-call id must not fail the session",
        "known_issue": 67,
    },
    {
        "name": "clean",
        "mode": "clean",
        "expect_status": "success",
        "expect_attempts": 1,
        "note": "baseline: one complete stream",
    },
    {
        "name": "truncate_once_mid_reasoning",
        "mode": "truncate_once",
        "expect_status": "success",
        "expect_attempts": 2,
        "note": "issue #62: truncated stream after reasoning must be retried",
    },
    {
        "name": "truncate_always",
        "mode": "truncate_always",
        "expect_status": "failed",
        "expect_attempts": 2,
        "expect_usage": True,
        "note": "#62 bound: retry once, then fail with usage preserved",
    },
    {
        "name": "truncate_silent_always",
        "mode": "truncate_silent_always",
        "expect_status": "failed",
        "expect_attempts": 2,
        "note": "no usage from the provider: failure must still be bounded",
    },
    {
        "name": "content_then_truncate_once",
        "mode": "content_truncate_once",
        "expect_status": "success",
        "expect_attempts": 2,
        "forbid_duplicate": True,
        "note": "visible content before truncation must not duplicate in the transcript",
    },
    {
        "name": "toolcall_then_truncate_once",
        "mode": "toolcall_truncate_once",
        "expect_status": "success",
        "expect_attempts": 2,
        "forbid_tool_call": True,
        "note": "a tool call from a truncated attempt must never execute",
    },
    {
        "name": "drop_once_mid_stream",
        "mode": "drop_once",
        "expect_status": "success",
        "expect_attempts": None,
        "note": "transport-level socket close must recover",
    },
    {
        "name": "http_429_once",
        "mode": "http_429_once",
        "expect_status": "success",
        "expect_attempts": None,
        "note": "rate limit must back off and retry",
    },
    {
        "name": "idle_stall_once",
        "mode": "stall_once",
        "expect_status": "success",
        "expect_attempts": None,
        "timeout": 900.0,
        "note": "a stall past the 300 s idle budget must be retried, not fatal",
    },
    {
        "name": "http_522_once",
        "mode": "http_522_once",
        "expect_status": "success",
        "expect_attempts": None,
        "note": "gateway 522 must be classified as a retryable server error",
        "known_issue": 68,
    },
    {
        "name": "http_500_once",
        "mode": "http_500_once",
        "expect_status": "success",
        "expect_attempts": None,
        "note": "server error must back off and retry",
    },
    {
        "name": "empty_completion_once",
        "mode": "empty_once",
        "expect_status": "success",
        "expect_attempts": None,
        "note": "empty response recovery instruction",
    },
]


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def parse_events(stdout: str) -> dict:
    """Summarise an orca exec JSONL stream."""
    events = []
    for line in stdout.splitlines():
        line = line.strip()
        if line.startswith("{"):
            try:
                events.append(json.loads(line))
            except json.JSONDecodeError:
                continue
    summary: dict = {"events": len(events)}
    for event in events:
        kind = event.get("type")
        payload = event.get("payload") or {}
        if kind == "session.completed":
            summary["status"] = payload.get("status")
            terminal = payload.get("terminal") or {}
            if isinstance(terminal, dict) and terminal:
                kind_name, detail = next(iter(terminal.items()))
                summary["terminal_kind"] = kind_name
                if isinstance(detail, dict):
                    summary["terminal_message"] = detail.get("message")
                    summary["terminal_class"] = detail.get("class")
                    summary["terminal_usage"] = detail.get("usage")
        elif kind == "usage.updated":
            summary["provider_usage"] = payload
        elif kind == "error":
            summary.setdefault("errors", []).append(payload.get("message"))
        elif kind == "item.completed":
            item = payload.get("item") or payload
            if isinstance(item, dict) and item.get("type") in (
                "assistant_message",
                "assistant.message",
            ):
                summary.setdefault("assistant_items", []).append(str(item.get("content"))[:200])
        elif kind == "tool.call.requested":
            summary.setdefault("tool_calls_requested", []).append(payload.get("name"))
    return summary


def run_scenario(binary: str, port: int, scenario: dict, timeout: float, out_file: Path) -> dict:
    prompt = scenario.get("tokens") or f"Reply with one short sentence. FIMODE={scenario['mode']}"
    with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as cwd:
        env = dict(os.environ)
        env["ORCA_HOME"] = home
        env.pop("DEEPSEEK_API_KEY", None)
        command = [
            binary,
            "exec",
            "--mode",
            "full-auto",
            "--output-format",
            "jsonl",
            "--base-url",
            f"http://127.0.0.1:{port}",
            "--api-key",
            "fault-injection-key",
            "--cwd",
            cwd,
            "--no-history",
            prompt,
        ]
        started = time.time()
        try:
            completed = subprocess.run(
                command, capture_output=True, text=True, timeout=timeout, env=env
            )
            stdout, stderr, code = completed.stdout, completed.stderr, completed.returncode
            timed_out = False
        except subprocess.TimeoutExpired as expired:
            stdout = expired.stdout or ""
            stderr = expired.stderr or ""
            if isinstance(stdout, bytes):
                stdout = stdout.decode(errors="replace")
            if isinstance(stderr, bytes):
                stderr = stderr.decode(errors="replace")
            code = None
            timed_out = True
    summary = parse_events(stdout)
    out_file.write_text(stdout, encoding="utf-8")
    summary["stdout_path"] = str(out_file)
    summary["committed_replies"] = stdout.count('"assistant_content": "Fault-injection reply."')
    summary.update(
        {
            "exit_code": code,
            "timed_out": timed_out,
            "duration_s": round(time.time() - started, 2),
            "marker_seen": TERMINAL_MARKER in stdout,
            "stderr_tail": stderr[-400:],
        }
    )
    return summary


def verdict(scenario: dict, result: dict, attempts: int) -> tuple[str, list[str]]:
    notes = []
    if scenario["expect_status"] is None:
        return "INFO", notes
    ok = True
    if result.get("status") != scenario["expect_status"]:
        ok = False
        notes.append(f"status={result.get('status')} expected {scenario['expect_status']}")
    if scenario["expect_attempts"] is not None and attempts != scenario["expect_attempts"]:
        ok = False
        notes.append(f"attempts={attempts} expected {scenario['expect_attempts']}")
    usage = result.get("provider_usage") or {}
    if scenario.get("expect_usage") and not usage.get("input_tokens"):
        ok = False
        notes.append("usage accounting lost for the failed attempt")
    if scenario.get("forbid_duplicate") and result.get("committed_replies", 0) > 1:
        ok = False
        notes.append(f"discarded attempt leaked ({result['committed_replies']} committed replies)")
    if scenario.get("forbid_tool_call") and result.get("tool_calls_requested"):
        ok = False
        notes.append(f"truncated attempt executed tools: {result['tool_calls_requested']}")
    return ("PASS" if ok else "FAIL"), notes


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/release/orca")
    parser.add_argument("--out", default="jobs/eval-fault-injection")
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
        for scenario in SCENARIOS:
            result = run_scenario(
                binary,
                port,
                scenario,
                scenario.get("timeout", args.timeout),
                out_dir / f"{scenario['name']}.jsonl",
            )
            entries = [
                json.loads(line)
                for line in request_log.read_text(encoding="utf-8").splitlines()
                if line.strip()
            ]
            mode = scenario.get("mode") or scenario["tokens"].split("FIMODE=")[-1].split()[0]
            attempts = len([e for e in entries if e["mode"] == mode])
            status, notes = verdict(scenario, result, attempts)
            record = {
                "scenario": scenario["name"],
                "mode": scenario.get("mode") or "tool_script",
                "note": scenario["note"],
                "verdict": status,
                "notes": notes,
                "attempts": attempts,
                "result": result,
            }
            results.append(record)
            marker = {"PASS": "PASS", "FAIL": "FAIL", "INFO": "INFO"}[status]
            usage = result.get("usage") or {}
            print(
                f"[{marker}] {scenario['name']:<28} status={result.get('status')!s:<8} "
                f"exit={result.get('exit_code')!s:<5} attempts={attempts:<2} "
                f"tokens={usage.get('input_tokens')}/{usage.get('output_tokens')} "
                f"{'; '.join(notes)}"
                + (f" (known issue #{scenario['known_issue']})" if status == "FAIL" and scenario.get("known_issue") else ""),
                flush=True,
            )
    finally:
        server.shutdown()

    report = {
        "binary": binary,
        "started_at": stamp,
        "scenarios": results,
        "summary": {
            "pass": len([r for r in results if r["verdict"] == "PASS"]),
            "fail": len([r for r in results if r["verdict"] == "FAIL"]),
            "info": len([r for r in results if r["verdict"] == "INFO"]),
        },
    }
    (out_dir / "report.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(f"\nreport: {out_dir / 'report.json'}")
    return 1 if report["summary"]["fail"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
