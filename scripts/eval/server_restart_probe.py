#!/usr/bin/env python3
"""Server-restart probe: what survives a crash mid-permission-request.

The evaluation plan lists "cross-process pending-route persistence" as uncovered:
`permission_probe.py` covers replay inside one process (first response wins, same
response is idempotent, conflicting response is refused, unknown id is refused), but
nothing checks what a *restarted* server does with state the previous process owned.

Scenario:

1. server A: `thread/start`, then a turn whose tool call asks for permission;
2. SIGKILL A while the permission request is still pending (client never answered);
3. server B on the same `ORCA_HOME`/workspace;
4. a `permission/respond` for A's request id must be refused cleanly, not executed;
5. `thread/resume` for A's thread must either restore it or report a clean error —
   a restarted server must never die because of a request it did not issue (see #79);
6. a fresh turn on B must still work.

Usage:
    python3 scripts/eval/server_restart_probe.py [--binary target/release/orca]
"""

from __future__ import annotations

import argparse
import json
import os
import select
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


class Server:
    """`orca --mode server` over stdio, with id-aware event reading."""

    def __init__(self, binary: str, port: int, cwd: str, env: dict) -> None:
        self.process = subprocess.Popen(
            [binary, "--mode", "server", "--api-key", "restart-probe",
             "--base-url", f"http://127.0.0.1:{port}"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=env,
            cwd=cwd,
            bufsize=1,
        )

    def send(self, payload: dict) -> bool:
        try:
            assert self.process.stdin
            self.process.stdin.write(json.dumps(payload) + "\n")
            self.process.stdin.flush()
            return True
        except (BrokenPipeError, ValueError):
            return False

    def read_event(self, seconds: float = 20.0, match=None) -> dict | None:
        deadline = time.time() + seconds
        while time.time() < deadline:
            ready, _, _ = select.select([self.process.stdout], [], [], 0.5)
            if not ready:
                if self.process.poll() is not None:
                    return None
                continue
            line = self.process.stdout.readline()
            if not line:
                return None
            line = line.strip()
            if not line.startswith("{"):
                continue
            event = json.loads(line)
            if match is None or event.get("event") in match or event.get("id") == match:
                return event
        return None

    def alive(self) -> bool:
        return self.process.poll() is None

    def kill(self) -> None:
        try:
            self.process.send_signal(signal.SIGKILL)
            self.process.wait(timeout=10)
        except Exception:
            self.process.kill()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/release/orca")
    parser.add_argument("--out", default="jobs/eval-server-restart-probe")
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    if not Path(binary).exists():
        print(f"binary not found: {binary}", file=sys.stderr)
        return 2

    stamp = time.strftime("%Y%m%d-%H%M%S")
    out_dir = Path(args.out) / stamp
    out_dir.mkdir(parents=True, exist_ok=True)
    port = free_port()
    provider = mock_provider.serve(port, str(out_dir / "provider-requests.jsonl"), "127.0.0.1")
    home = tempfile.mkdtemp(prefix="orca-restart-home-")
    work = tempfile.mkdtemp(prefix="orca-restart-work-")
    env = dict(os.environ)
    env["ORCA_HOME"] = home
    env.pop("DEEPSEEK_API_KEY", None)

    checks: list[tuple[str, bool, str, str]] = []
    results: list[dict] = []

    def record(name: str, ok: bool, detail: str, issue: str = "") -> None:
        checks.append((name, ok, detail, issue))
        results.append({"case": name, "ok": ok, "detail": detail, "issue": issue})

    first = Server(binary, port, work, env)
    second: Server | None = None
    try:
        first.send({"id": "thread", "method": "thread/start", "params": {}})
        started = first.read_event(20.0, match="thread_started")
        thread_id = (started or {}).get("threadId")
        record("thread/start on server A", bool(thread_id), f"threadId={thread_id}")

        request_id = None
        if thread_id:
            first.send({
                "id": "turn",
                "method": "turn/start",
                "params": {
                    "threadId": thread_id,
                    "input": [{"type": "text", "text": "FIMODE=tool_script FITOOL=request_permissions"}],
                },
            })
            permission = first.read_event(45.0, match="permission_request")
            request_id = (permission or {}).get("requestId")
            record("permission request pending on server A", bool(request_id), f"requestId={request_id}")

        first.kill()
        record("server A killed mid-request", not first.alive(), f"rc={first.process.poll()}")

        second = Server(binary, port, work, env)
        record("server B starts on the same ORCA_HOME", second.alive(), f"rc={second.process.poll()}")

        if request_id:
            second.send({"id": "respond", "method": "permission/respond",
                         "params": {"requestId": request_id, "decision": "allow", "scope": "turn"}})
            reply = second.read_event(20.0, match="respond")
            ok = bool(reply) and reply.get("event") == "error"
            record(
                "stale permission/respond refused cleanly",
                ok,
                f"reply={json.dumps(reply)[:130]}",
            )
            record("server B survived the stale response", second.alive(), f"rc={second.process.poll()}")

        if thread_id:
            second.send({"id": "resume", "method": "thread/resume", "params": {"threadId": thread_id}})
            resumed = second.read_event(30.0, match="resume")
            event = (resumed or {}).get("event")
            ok = event in ("thread_started", "error")
            record(
                "resume after restart: reply, no fatality",
                ok,
                f"event={event} detail={json.dumps(resumed)[:110]}",
                issue="" if ok else "blocked by #79",
            )
            record("server B alive after resume", second.alive(), f"rc={second.process.poll()}",
                   issue="" if second.alive() else "blocked by #79")

        if second.alive():
            second.send({"id": "fresh", "method": "thread/start", "params": {}})
            fresh = second.read_event(20.0, match="thread_started")
            record("server B still serves new threads", (fresh or {}).get("event") == "thread_started",
                   f"event={(fresh or {}).get('event')}")
    finally:
        first.kill()
        if second:
            second.kill()
        provider.shutdown()

    (out_dir / "checks.jsonl").write_text("\n".join(json.dumps(item) for item in results) + "\n")
    failures = 0
    for name, ok, detail, issue in checks:
        failures += 0 if ok else 1
        suffix = f" — {issue}" if issue and not ok else ""
        print(f"[{'PASS' if ok else 'FAIL'}] {name:<44} {detail[:100]}{suffix}")
    print(f"\nlogs: {out_dir}")
    print(f"verdict: {'PASS' if not failures else f'FAIL ({failures} checks)'}")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
