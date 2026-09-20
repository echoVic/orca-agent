#!/usr/bin/env python3
"""Command-lifecycle contract probes (L1 in docs/evaluation-plan.md).

Drives `orca exec` against scripts/eval/mock_provider.py and records what the
`bash` tool actually does for pipe/pty commands, default vs explicit
`yield_time_ms`. This is the regression harness for issue #63
(DEFAULT_YIELD_TIME_MS) and for the cancel/timeout semantics around it.

Usage:
    python3 scripts/eval/command_contract.py [--binary target/release/orca]
"""

from __future__ import annotations

import argparse
import json
import os
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
        "name": "pipe_default_short",
        "tokens": "FIMODE=tool_bash FISLEEP=3",
        "expect_first_state": "completed",
        "expect_first_elapsed_max": 8.0,
        "note": "3s command is inside the 10s pipe default and returns inline",
    },
    {
        "name": "pipe_default_long",
        "tokens": "FIMODE=tool_bash FISLEEP=25",
        "expect_first_state": "running",
        "expect_first_elapsed_min": 8.0,
        "expect_first_elapsed_max": 15.0,
        "expect_task_wait_min": 1,
        "note": "25s command yields after ~10s, then task_wait finishes it",
    },
    {
        "name": "pipe_explicit_zero",
        "tokens": "FIMODE=tool_bash FISLEEP=3 FIYIELD=0",
        "expect_first_state": "running",
        "expect_first_elapsed_max": 2.0,
        "expect_task_wait_min": 1,
        "note": "explicit yield_time_ms=0 returns as soon as the task is registered",
    },
    {
        "name": "pipe_explicit_long",
        "tokens": "FIMODE=tool_bash FISLEEP=3 FIYIELD=20000",
        "expect_first_state": "completed",
        "expect_first_elapsed_max": 8.0,
        "note": "explicit yield_time_ms=20000 still overrides the default",
    },
    {
        "name": "pty_default_short",
        "tokens": "FIMODE=tool_bash FISLEEP=3 FIPty=1",
        "expect_first_state": "running",
        "expect_first_elapsed_max": 3.5,
        "expect_task_wait_min": 1,
        "note": "pty keeps the responsive 1s default",
    },
    {
        "name": "interactive_pty_input",
        "tokens": "FIMODE=tool_script FITOOL=interactive",
        "expect_first_state": "running",
        "expect_sequence_contains": "task_send_input",
        "note": "a pty command waiting on stdin must accept task_send_input and finish",
        "expect_output_contains": "got:hello",
    },
    {
        "name": "flood_output_memory",
        "tokens": "FIMODE=tool_script FITOOL=flood",
        "expect_first_state": "running",
        "sample_rss": True,
        "note": "`yes` under task_stop: output must be drained, not buffered without bound",
    },
    {
        "name": "orphan_after_session",
        "tokens": "FIMODE=tool_script FITOOL=orphan",
        "expect_first_state": "running",
        "post_run_orphan_check": True,
        "note": "a task-lifetime command must not outlive the session that owns it",
    },
    {
        "name": "timeout_kills_command",
        "tokens": "FIMODE=tool_script FITOOL=timeout",
        "expect_first_state_any": ["completed", "failed"],
        "expect_first_exit_nonzero": True,
        "expect_first_elapsed_max": 12.0,
        "note": "timeout_ms=2000 on `sleep 30` must terminate the command, not hang",
    },
    {
        "name": "stop_running_command",
        "tokens": "FIMODE=tool_script FITOOL=stop",
        "expect_first_state": "running",
        "expect_sequence_contains": "task_stop",
        "expect_last_state_any": ["completed", "failed", "stopped", "cancelled"],
        "note": "task_stop must report a truthful terminal for a running command",
    },
    {
        "name": "huge_output_bounded",
        "tokens": "FIMODE=tool_script FITOOL=bigout",
        "expect_first_state": "running",
        "expect_first_elapsed_max": 5.0,
        "expect_first_output_bounded": True,
        "note": "400k-line output returns one bounded page + cursor, never a full dump",
    },
]


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def run_case(binary: str, port: int, case: dict, timeout: float, out_file: Path) -> dict:
    prompt = f"Run the requested command. {case['tokens']}"
    ticks = Path("/tmp/fi-orphan-ticks")
    if ticks.exists():
        ticks.unlink()
    post: dict = {}
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
            "contract-key",
            "--cwd",
            cwd,
            "--no-history",
            prompt,
        ]
        started = time.time()
        peak_rss_kb = 0
        if case.get("sample_rss"):
            # Redirect to files: unread pipes make the *runner* block once the
            # child fills the 64 KiB pipe buffer, which would fake a hang.
            with open(out_file.with_suffix(".stdout"), "w", encoding="utf-8") as sink:
                process = subprocess.Popen(
                    command, stdout=sink, stderr=subprocess.STDOUT, text=True, env=env
                )
                deadline = started + timeout
                while process.poll() is None and time.time() < deadline:
                    try:
                        rss = subprocess.run(
                            ["ps", "-o", "rss=", "-p", str(process.pid)],
                            capture_output=True,
                            text=True,
                        ).stdout.strip()
                        if rss.isdigit():
                            peak_rss_kb = max(peak_rss_kb, int(rss))
                    except OSError:
                        pass
                    time.sleep(0.25)
                if process.poll() is None:
                    process.kill()
                process.wait()
                code = process.returncode
            stdout = out_file.with_suffix(".stdout").read_text(encoding="utf-8")
        else:
            try:
                completed = subprocess.run(
                    command, capture_output=True, text=True, timeout=timeout, env=env
                )
                stdout, code = completed.stdout, completed.returncode
            except subprocess.TimeoutExpired as expired:
                stdout = expired.stdout or ""
                if isinstance(stdout, bytes):
                    stdout = stdout.decode(errors="replace")
                code = None
    out_file.write_text(stdout, encoding="utf-8")

    if case.get("post_run_orphan_check"):
        import subprocess as sp

        time.sleep(2)
        first_count = len(ticks.read_text().splitlines()) if ticks.exists() else 0
        time.sleep(5)
        second_count = len(ticks.read_text().splitlines()) if ticks.exists() else 0
        alive = sp.run(
            ["pgrep", "-fl", "FI_ORPHAN_"], capture_output=True, text=True
        ).stdout.strip()
        post = {
            "orphan_ticks_after_2s": first_count,
            "orphan_ticks_after_7s": second_count,
            "orphan_still_running": second_count > first_count,
            "orphan_processes": alive.splitlines()[:3],
        }

    requested: dict[str, dict] = {}
    calls: list[dict] = []
    status = None
    for line in stdout.splitlines():
        line = line.strip()
        if not line.startswith("{"):
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        kind = event.get("type")
        payload = event.get("payload") or {}
        if kind == "tool.call.requested":
            requested[payload.get("id")] = {
                "name": payload.get("name"),
                "ts": event.get("timestamp_ms"),
            }
        elif kind == "tool.call.completed":
            origin = requested.get(payload.get("id")) or {}
            detail: dict = {}
            # Failed calls carry the structured result inside `error`; successful
            # ones inside `output`.
            for field in ("output", "error"):
                candidate = payload.get(field)
                if isinstance(candidate, str) and candidate.strip().startswith("{"):
                    try:
                        detail = json.loads(candidate)
                        break
                    except json.JSONDecodeError:
                        detail = {}
            calls.append(
                {
                    "name": payload.get("name") or origin.get("name"),
                    "state": detail.get("state") or payload.get("status"),
                    "return_reason": detail.get("return_reason"),
                    "exit_code": detail.get("exit_code"),
                    "truncated": payload.get("truncated"),
                    "output_bytes": len(detail.get("output") or ""),
                    "output_text": str(detail.get("output") or "")[:400],
                    "elapsed_s": (
                        round((event.get("timestamp_ms") - origin["ts"]) / 1000, 2)
                        if origin.get("ts") and event.get("timestamp_ms")
                        else None
                    ),
                }
            )
        elif kind == "session.completed":
            status = payload.get("status")

    bash_calls = [call for call in calls if call["name"] == "bash"]
    task_waits = [call for call in calls if call["name"] == "task_wait"]
    return {
        "stdout_path": str(out_file),
        "exit_code": code,
        "status": status,
        "turns": len([1 for line in stdout.splitlines() if '"turn.started"' in line]),
        "wall_s": round(time.time() - started, 2),
        "bash_calls": bash_calls,
        "task_wait_calls": len(task_waits),
        "first": bash_calls[0] if bash_calls else None,
        "all_calls": calls,
        "last_call": calls[-1] if calls else None,
        "tool_sequence": [call["name"] for call in calls],
        "post_run": post,
        "peak_rss_mb": round(peak_rss_kb / 1024, 1) if peak_rss_kb else None,
    }


def verdict(case: dict, result: dict) -> tuple[str, list[str]]:
    notes: list[str] = []
    ok = True
    if result["status"] != "success":
        ok = False
        notes.append(f"session status={result['status']}")
    first = result["first"]
    if not first:
        return "FAIL", notes + ["no bash call observed"]
    if case.get("expect_first_state") and first["state"] != case["expect_first_state"]:
        ok = False
        notes.append(f"first state={first['state']} expected {case['expect_first_state']}")
    if case.get("expect_first_state_any") and first["state"] not in case["expect_first_state_any"]:
        ok = False
        notes.append(f"first state={first['state']} not in {case['expect_first_state_any']}")
    if case.get("expect_first_exit_nonzero") and not first.get("exit_code"):
        ok = False
        notes.append(f"exit_code={first.get('exit_code')} for a killed command")
    if (
        case.get("expect_sequence_contains")
        and case["expect_sequence_contains"] not in result["tool_sequence"]
    ):
        ok = False
        notes.append(f"sequence missing {case['expect_sequence_contains']}")
    if case.get("expect_last_state_any"):
        last_state = result["last_call"]["state"] if result.get("last_call") else None
        if last_state not in case["expect_last_state_any"]:
            ok = False
            notes.append(f"last state={last_state} not in {case['expect_last_state_any']}")
    post = result.get("post_run") or {}
    if case.get("post_run_orphan_check"):
        if post.get("orphan_still_running"):
            ok = False
            notes.append(
                f"command outlived the session (ticks {post.get('orphan_ticks_after_2s')}"
                f" -> {post.get('orphan_ticks_after_7s')})"
            )
        elif post.get("orphan_ticks_after_7s"):
            notes.append("command was stopped with the session")
    if case.get("sample_rss"):
        peak = result.get("peak_rss_mb")
        if peak is not None and peak > 600:
            ok = False
            notes.append(f"RSS grew to {peak} MB while draining unbounded output")
        elif peak is not None:
            notes.append(f"peak RSS {peak} MB")
    if case.get("expect_output_contains"):
        joined = " ".join(call.get("output_text", "") for call in result.get("all_calls", []))
        if case["expect_output_contains"] not in joined:
            ok = False
            notes.append(f"missing output {case['expect_output_contains']!r}")
    if case.get("expect_first_output_bounded"):
        bytes_out = first.get("output_bytes") or 0
        if bytes_out > 2_000_000:
            ok = False
            notes.append(f"unbounded output ({bytes_out} bytes returned)")
    elapsed = first.get("elapsed_s")
    if elapsed is None:
        ok = False
        notes.append("no elapsed time")
    else:
        if case.get("expect_first_elapsed_min") and elapsed < case["expect_first_elapsed_min"]:
            ok = False
            notes.append(f"yielded too early ({elapsed}s)")
        if case.get("expect_first_elapsed_max") and elapsed > case["expect_first_elapsed_max"]:
            ok = False
            notes.append(f"waited too long ({elapsed}s)")
    if case.get("expect_task_wait_min") and result["task_wait_calls"] < case["expect_task_wait_min"]:
        ok = False
        notes.append(f"task_wait calls={result['task_wait_calls']}")
    return ("PASS" if ok else "FAIL"), notes


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/release/orca")
    parser.add_argument("--out", default="jobs/eval-command-contract")
    parser.add_argument("--timeout", type=float, default=180.0)
    args = parser.parse_args()

    binary = str(Path(args.binary).resolve())
    if not Path(binary).exists():
        print(f"binary not found: {binary}", file=sys.stderr)
        return 2

    stamp = time.strftime("%Y%m%d-%H%M%S")
    out_dir = Path(args.out) / stamp
    out_dir.mkdir(parents=True, exist_ok=True)
    port = free_port()
    server = mock_provider.serve(port, str(out_dir / "requests.jsonl"))
    results = []
    try:
        for case in CASES:
            result = run_case(binary, port, case, args.timeout, out_dir / f"{case['name']}.jsonl")
            status, notes = verdict(case, result)
            results.append(
                {"case": case["name"], "note": case["note"], "verdict": status,
                 "notes": notes, "result": result}
            )
            first = result["first"] or {}
            print(
                f"[{status}] {case['name']:<22} status={result['status']!s:<8} "
                f"first={first.get('state')!s:<10} elapsed={first.get('elapsed_s')!s:<6} "
                f"task_wait={result['task_wait_calls']} turns={result['turns']} "
                f"{'; '.join(notes)}",
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
