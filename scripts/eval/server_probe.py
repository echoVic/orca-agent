#!/usr/bin/env python3
"""Server (JSON-RPC over stdio) surface probe.

Covers the protocol IDE clients and embeds use — `--mode server` — which the
Terminal-Bench suites never touch: thread lifecycle, a full turn driven by a model
that calls a tool, direct `command/exec`, unknown methods and malformed input.

Usage:
    python3 scripts/eval/server_probe.py [--binary target/release/orca]
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import select
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import mock_provider  # noqa: E402


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


class Server:
    def __init__(self, binary: str, port: int, cwd: str, env: dict) -> None:
        self.process = subprocess.Popen(
            [
                binary, "--mode", "server", "--api-key", "server-probe",
                "--base-url", f"http://127.0.0.1:{port}",
            ],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=env,
            cwd=cwd,
            bufsize=1,
        )

    def send_raw(self, text: str) -> None:
        assert self.process.stdin
        self.process.stdin.write(text)
        self.process.stdin.flush()

    def exchange(self, request: dict, seconds: float = 30.0) -> list[dict]:
        self.send_raw(json.dumps(request) + "\n")
        collected: list[dict] = []
        deadline = time.time() + seconds
        while time.time() < deadline:
            ready, _, _ = select.select([self.process.stdout], [], [], 0.5)
            if not ready:
                continue
            line = self.process.stdout.readline()
            if not line:
                break
            line = line.strip()
            if not line.startswith("{"):
                continue
            event = json.loads(line)
            collected.append(event)
            if event.get("event") == "turn_completed" or (
                event.get("id") == request.get("id") and event.get("event") == "error"
            ):
                break
        return collected

    def close(self) -> None:
        try:
            if self.process.stdin:
                self.process.stdin.close()
            self.process.wait(timeout=10)
        except (subprocess.TimeoutExpired, BrokenPipeError):
            self.process.kill()


def acp_smoke(binary: str, port: int, env: dict, cwd: str) -> tuple[bool, bool, list[str]]:
    """initialize + session/new on the ACP surface (IDE integration path)."""
    process = subprocess.Popen(
        [binary, "--mode", "acp", "--api-key", "acp-probe", "--base-url", f"http://127.0.0.1:{port}"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=env,
        cwd=cwd,
        bufsize=1,
    )

    def call(request: dict, seconds: float = 20.0) -> dict | None:
        assert process.stdin
        process.stdin.write(json.dumps(request) + "\n")
        process.stdin.flush()
        deadline = time.time() + seconds
        while time.time() < deadline:
            ready, _, _ = select.select([process.stdout], [], [], 0.5)
            if not ready:
                continue
            line = process.stdout.readline()
            if not line:
                break
            line = line.strip()
            if not line.startswith("{"):
                continue
            message = json.loads(line)
            if message.get("id") == request.get("id"):
                return message
        return None

    try:
        init = call(
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": 1,
                    "clientCapabilities": {
                        "fs": {"readTextFile": False, "writeTextFile": False}
                    },
                    "clientInfo": {"name": "server-probe", "version": "0.0.1"},
                },
            }
        )
        capabilities = ((init or {}).get("result") or {}).get("agentCapabilities") or {}
        session = call(
            {"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {"cwd": cwd, "mcpServers": []}}
        )
        session_id = ((session or {}).get("result") or {}).get("sessionId")
        warnings = (
            (((session or {}).get("result") or {}).get("_meta") or {})
            .get("orca.dev/readiness", {})
            .get("startupWarnings", [])
        )
        return bool(capabilities), bool(session_id), warnings
    finally:
        try:
            if process.stdin:
                process.stdin.close()
            process.wait(timeout=10)
        except (subprocess.TimeoutExpired, BrokenPipeError):
            process.kill()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/release/orca")
    parser.add_argument("--out", default="jobs/eval-server-probe")
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    if not Path(binary).exists():
        print(f"binary not found: {binary}", file=sys.stderr)
        return 2

    port = free_port()
    server = mock_provider.serve(port, f"{args.out}-requests.jsonl")
    env = dict(os.environ)
    env["ORCA_HOME"] = tempfile.mkdtemp()
    env.pop("DEEPSEEK_API_KEY", None)
    cwd = tempfile.mkdtemp()
    client = Server(binary, port, cwd, env)
    failures = 0
    try:
        # 1. thread lifecycle
        thread_events = client.exchange({"id": "t1", "method": "thread/start", "params": {}})
        thread_id = next(
            (e.get("threadId") for e in thread_events if e.get("event") == "thread_started"), None
        )
        ok = bool(thread_id)
        failures += 0 if ok else 1
        print(f"[{'PASS' if ok else 'FAIL'}] thread/start            threadId={thread_id}")

        # 2. a full turn whose model calls a tool
        command = base64.b64encode(b"echo SERVER-TURN-OK").decode()
        prompt = f"FIMODE=tool_script FITOOL=cmd FICMD64={command}"
        turn_events = client.exchange(
            {
                "id": "t2",
                "method": "turn/start",
                "params": {"threadId": thread_id, "input": [{"type": "text", "text": prompt}]},
            }
        )
        names = [e.get("event") for e in turn_events]
        completed = next((e for e in turn_events if e.get("event") == "turn_completed"), None)
        tool_ok = any(e.get("event") == "tool_completed" for e in turn_events)
        ok = completed is not None and tool_ok
        failures += 0 if ok else 1
        print(
            f"[{'PASS' if ok else 'FAIL'}] turn/start + tool call  events={names[:6]} "
            f"status={completed.get('status') if completed else None}"
        )

        # 3. command/exec (needs an OS sandbox on this host; report the env limitation)
        exec_events = client.exchange(
            {
                "id": "c1",
                "method": "command/exec",
                "params": {"command": ["sh", "-lc", "echo SERVER-EXEC-OK"]},
            }
        )
        exec_error = next((e for e in exec_events if e.get("event") == "error"), None)
        if exec_error and "no OS-enforced sandbox backend" in str(exec_error.get("message")):
            print(f"[INFO] command/exec           env-limited: {exec_error['message'][:90]}")
        else:
            ok = exec_error is None
            failures += 0 if ok else 1
            print(f"[{'PASS' if ok else 'FAIL'}] command/exec            events={exec_events[:1]}")

        # 4. unknown method and malformed input must be structured errors, not crashes
        bad = client.exchange({"id": "b1", "method": "nope/nope", "params": {}})
        ok = bool(bad) and bad[0].get("event") == "error"
        failures += 0 if ok else 1
        print(f"[{'PASS' if ok else 'FAIL'}] unknown method          {str(bad[0].get('message'))[:60] if bad else 'no reply'}")

        client.send_raw("not json at all\n")
        time.sleep(0.5)
        alive = client.exchange({"id": "a1", "method": "thread/start", "params": {}})
        ok = any(e.get("event") == "thread_started" for e in alive)
        failures += 0 if ok else 1
        print(f"[{'PASS' if ok else 'FAIL'}] malformed input         server still serving={ok}")

        # 5. ACP surface: initialize + session/new (IDE integration path)
        capabilities, session_ok, warnings = acp_smoke(binary, port, env, cwd)
        ok = capabilities and session_ok
        failures += 0 if ok else 1
        print(
            f"[{'PASS' if ok else 'FAIL'}] acp initialize/new     capabilities={capabilities} "
            f"session={session_ok} warnings={len(warnings)}"
        )
        if warnings:
            print(f"[INFO] acp startup warning   {str(warnings[0])[:100]}")
    finally:
        client.close()
        server.shutdown()

    print(f"\nverdict: {'PASS' if not failures else 'FAIL'}")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
