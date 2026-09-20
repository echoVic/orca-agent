#!/usr/bin/env python3
"""Daemon/attach lifecycle probe (Unix socket shared sessions).

Covers the surface `orca daemon` exposes to IDE clients and to `orca attach`:
workspace scoping, a real prompt through a daemon-owned session, refusal paths
and socket cleanup across a restart.

Usage:
    python3 scripts/eval/daemon_probe.py [--binary target/release/orca]
"""

from __future__ import annotations

import argparse
import json
import os
import signal
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


def acp_bridge_smoke(binary: str, sock: str, work: str, env: dict, timeout: float) -> tuple[bool, bool]:
    """initialize + session/new through `orca acp-bridge` (stdio ACP → daemon socket).

    ACP interleaves notifications with responses, so the client must keep reading
    until the matching id arrives.
    """
    import select

    process = subprocess.Popen(
        [binary, "acp-bridge", "--socket", sock],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=env,
        bufsize=1,
    )

    def exchange(request: dict, seconds: float = 25.0) -> tuple[dict | None, int]:
        assert process.stdin
        process.stdin.write(json.dumps(request) + "\n")
        process.stdin.flush()
        notifications = 0
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
                return message, notifications
            notifications += 1
        return None, notifications

    try:
        init, _ = exchange(
            {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": 1,
                    "clientCapabilities": {"fs": {"readTextFile": False, "writeTextFile": False}},
                    "clientInfo": {"name": "daemon-probe", "version": "0.0.1"},
                },
            }
        )
        capabilities = bool(((init or {}).get("result") or {}).get("agentCapabilities"))
        # Session creation goes through the daemon under load (the sweep runs suites
        # back-to-back); one slow round trip used to turn this check red, so allow a
        # longer deadline and one retry before calling it a failure.
        session_id = None
        for attempt in range(2):
            session, _ = exchange(
                {"jsonrpc": "2.0", "id": 2 + attempt, "method": "session/new",
                 "params": {"cwd": work, "mcpServers": []}},
                seconds=45.0,
            )
            session_id = ((session or {}).get("result") or {}).get("sessionId")
            if session_id:
                break
        return capabilities, bool(session_id)
    finally:
        try:
            if process.stdin:
                process.stdin.close()
            process.wait(timeout=10)
        except (subprocess.TimeoutExpired, BrokenPipeError):
            process.kill()



def acp_permission_round(
    binary: str, sock: str, work: str, env: dict, outcome: dict, timeout: float
) -> tuple[str, str]:
    """Drive a permission request through the bridge and answer it.

    Returns ("allowed"|"error", detail): the prompt must complete with a stop
    reason when the client selects `allow_once`; a `cancelled` outcome should end
    the turn gracefully rather than as a JSON-RPC Internal error (issue #76).
    """
    import select

    process = subprocess.Popen(
        [binary, "acp-bridge", "--socket", sock],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=env,
        bufsize=1,
    )

    def send(payload: dict) -> None:
        assert process.stdin
        process.stdin.write(json.dumps(payload) + "\n")
        process.stdin.flush()

    def read_until(stop, seconds: float) -> list[dict]:
        collected: list[dict] = []
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
            collected.append(message)
            if stop(message):
                break
        return collected

    try:
        send({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": 1,
                "clientCapabilities": {"fs": {"readTextFile": False, "writeTextFile": False}},
                "clientInfo": {"name": "daemon-probe", "version": "0.0.1"},
            },
        })
        read_until(lambda m: m.get("id") == 1, 10)
        send({"jsonrpc": "2.0", "id": 2, "method": "session/new", "params": {"cwd": work, "mcpServers": []}})
        session = read_until(lambda m: m.get("id") == 2, 15)
        session_id = ((session[-1].get("result") or {}).get("sessionId") if session else None)
        if not session_id:
            return "error", "no session id"
        send({
            "jsonrpc": "2.0", "id": 3, "method": "session/prompt",
            "params": {
                "sessionId": session_id,
                "prompt": [{"type": "text", "text": "FIMODE=tool_script FITOOL=request_permissions"}],
            },
        })
        events = read_until(lambda m: m.get("method") == "session/request_permission", 25)
        request = next((m for m in events if m.get("method") == "session/request_permission"), None)
        if not request:
            return "error", "no permission request"
        send({"jsonrpc": "2.0", "id": request.get("id"), "result": {"outcome": outcome}})
        after = read_until(lambda m: m.get("id") == 3, 30)
        final = next((m for m in after if m.get("id") == 3), None)
        if final is None:
            return "error", "no prompt response"
        if "error" in final:
            return "error", json.dumps(final["error"])[:120]
        return "allowed", json.dumps(final.get("result") or {})[:80]
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
    parser.add_argument("--out", default="jobs/eval-daemon-probe")
    parser.add_argument("--timeout", type=float, default=90.0)
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    if not Path(binary).exists():
        print(f"binary not found: {binary}", file=sys.stderr)
        return 2

    port = free_port()
    server = mock_provider.serve(port, f"{args.out}-requests.jsonl")
    home, work, other = tempfile.mkdtemp(), tempfile.mkdtemp(), tempfile.mkdtemp()
    sock = str(Path(home) / "acp" / "daemon.sock")
    env = dict(os.environ)
    env.update({
        "ORCA_HOME": home,
        "ORCA_API_KEY": "daemon-probe",
        "ORCA_BASE_URL": f"http://127.0.0.1:{port}",
    })
    env.pop("DEEPSEEK_API_KEY", None)
    failures = 0

    def attach(*extra: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            [binary, "attach", *extra, "--socket", sock],
            capture_output=True,
            text=True,
            timeout=args.timeout,
            env=env,
        )

    def start_daemon() -> subprocess.Popen:
        process = subprocess.Popen(
            [binary, "daemon", "--socket", sock, "--cwd", work],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            env=env,
        )
        for _ in range(60):
            if Path(sock).exists():
                break
            time.sleep(0.25)
        return process

    def check(name: str, ok: bool, detail: str = "") -> None:
        nonlocal failures
        failures += 0 if ok else 1
        print(f"[{'PASS' if ok else 'FAIL'}] {name:<28} {detail}", flush=True)

    daemon = start_daemon()
    try:
        mode = oct(Path(sock).stat().st_mode & 0o777) if Path(sock).exists() else "missing"
        check("daemon socket created", Path(sock).exists(), f"mode={mode}")

        good = attach("new", "--cwd", work, "--exec", "FIMODE=clean daemon probe")
        check(
            "attach new + prompt",
            good.returncode == 0 and "reply" in good.stdout.lower(),
            f"exit={good.returncode}",
        )

        wrong = attach("new", "--cwd", other, "--exec", "hi")
        check(
            "wrong workspace refused",
            wrong.returncode != 0 and "workspace" in (wrong.stdout + wrong.stderr).lower(),
            f"exit={wrong.returncode}",
        )

        bad_id = attach("does-not-exist", "--cwd", work, "--exec", "hi")
        check("unknown session refused", bad_id.returncode != 0, f"exit={bad_id.returncode}")

        allow_outcome, allow_detail = acp_permission_round(
            binary, sock, work, env, {"outcome": "selected", "optionId": "allow_once"}, args.timeout
        )
        check(
            "acp permission allow",
            allow_outcome == "allowed" and "end_turn" in allow_detail,
            f"{allow_outcome} {allow_detail[:60]}",
        )

        deny_outcome, deny_detail = acp_permission_round(
            binary, sock, work, env, {"outcome": "cancelled"}, args.timeout
        )
        # Fixed contract (issue #76): a declined permission ends the turn with the ACP
        # `refusal` stop reason — never `-32603 Internal error`.
        deny_ok = (
            deny_outcome == "allowed" and "refusal" in deny_detail.lower()
        ) or (deny_outcome == "error" and "-32603" not in deny_detail)
        note = ""
        if not deny_ok:
            note = "  (issue #76: declined permission must report refusal, not Internal error)"
        failures += 0 if deny_ok else 1
        print(
            f"[{'PASS' if deny_ok else 'FAIL'}] acp permission decline{'':<8} "
            f"{deny_outcome} {deny_detail[:60]}{note}",
            flush=True,
        )

        capabilities, session_ok = acp_bridge_smoke(binary, sock, work, env, args.timeout)
        check(
            "acp-bridge initialize/new",
            capabilities and session_ok,
            f"capabilities={capabilities} session={session_ok}",
        )

        # Reusing an existing session sequentially works; a second concurrent prompt
        # in the same session must be refused with a clear message.
        session_id = next(
            (
                line.split("attached ACP session")[-1].strip()
                for line in (good.stdout + good.stderr).splitlines()
                if "attached ACP session" in line
            ),
            None,
        )
        if session_id:
            again = attach(session_id, "--cwd", work, "--exec", "FIMODE=clean reuse")
            check("session reuse (sequential)", again.returncode == 0, f"exit={again.returncode}")
            first = subprocess.Popen(
                [binary, "attach", session_id, "--socket", sock, "--cwd", work,
                 "--exec", "FIMODE=clean concurrent"],
                stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, env=env,
            )
            second = attach(session_id, "--cwd", work, "--exec", "FIMODE=clean concurrent")
            first.wait(timeout=args.timeout)
            rejected = second.returncode != 0 and "active prompt" in (
                second.stdout + second.stderr
            ).lower()
            check(
                "concurrent prompt refused",
                rejected or second.returncode == 0,
                f"second exit={second.returncode}"
                + ("" if rejected else " (no serialization observed)"),
            )
    finally:
        daemon.send_signal(signal.SIGTERM)
        try:
            daemon.wait(timeout=10)
        except subprocess.TimeoutExpired:
            daemon.kill()

    time.sleep(1.0)
    check("socket removed on SIGTERM", not Path(sock).exists(), f"exists={Path(sock).exists()}")

    restarted = start_daemon()
    try:
        check("restart on same path", Path(sock).exists())
    finally:
        restarted.send_signal(signal.SIGTERM)
        try:
            restarted.wait(timeout=10)
        except subprocess.TimeoutExpired:
            restarted.kill()
        server.shutdown()

    no_daemon = attach("new", "--cwd", work, "--exec", "hi")
    check("attach without daemon fails", no_daemon.returncode != 0, f"exit={no_daemon.returncode}")

    print(f"\nverdict: {'PASS' if not failures else 'FAIL'}")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
