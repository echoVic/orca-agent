#!/usr/bin/env python3
"""ACP surface probe: hostile / malformed input on the IDE-facing protocol.

`daemon_probe.py` covers the happy path (initialize → session/new → prompt → permission).
This suite checks the **failure** paths an IDE client can actually produce, over
`orca acp-bridge` (stdio ACP → daemon socket):

* JSON-RPC framing: unknown method, malformed params, requests before `initialize`,
  explicit `id: null`, missing `id` — every *response* must echo the request id and use a
  sensible JSON-RPC error code (not `Internal error`).
* Unknown session ids for `session/prompt`, `session/set_model`, `session/cancel`.
* The bridge must survive all of it and still serve a valid session afterwards.

Usage:
    python3 scripts/eval/acp_param_probe.py [--binary target/release/orca]
"""

from __future__ import annotations

import argparse
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


class Bridge:
    """`orca acp-bridge` child, with id-correlated JSON-RPC exchange."""

    def __init__(self, binary: str, sock: str, env: dict, work: str) -> None:
        self.process = subprocess.Popen(
            [binary, "acp-bridge", "--socket", sock],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            env=env,
            cwd=work,
            bufsize=1,
        )
        self.notifications: list[dict] = []

    def send(self, request: dict) -> bool:
        try:
            assert self.process.stdin
            self.process.stdin.write(json.dumps(request) + "\n")
            self.process.stdin.flush()
            return True
        except BrokenPipeError:
            return False

    def await_id(self, request_id, seconds: float = 20.0) -> dict | None:
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
            message = json.loads(line)
            if "id" in message and message.get("id") == request_id:
                return message
            self.notifications.append(message)
        return None

    def alive(self) -> bool:
        return self.process.poll() is None

    def close(self) -> None:
        try:
            if self.process.stdin:
                self.process.stdin.close()
            self.process.wait(timeout=10)
        except (subprocess.TimeoutExpired, BrokenPipeError):
            self.process.kill()


def describe(message: dict | None) -> str:
    if message is None:
        return "no reply"
    if "error" in message:
        error = message.get("error") or {}
        return f"error id={message.get('id')!r} code={error.get('code')} msg={str(error.get('message'))[:60]!r}"
    result = message.get("result")
    return f"result id={message.get('id')!r} keys={sorted(result)[:4] if isinstance(result, dict) else type(result).__name__}"


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/release/orca")
    parser.add_argument("--out", default="jobs/eval-acp-param-probe")
    parser.add_argument("--timeout", type=float, default=60.0)
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    if not Path(binary).exists():
        print(f"binary not found: {binary}", file=sys.stderr)
        return 2

    stamp = time.strftime("%Y%m%d-%H%M%S")
    out_dir = Path(args.out) / stamp
    out_dir.mkdir(parents=True, exist_ok=True)

    port = free_port()
    provider = mock_provider.serve(port, str(out_dir / "provider-requests.jsonl"))
    home, work = tempfile.mkdtemp(prefix="orca-acp-home-"), tempfile.mkdtemp(prefix="orca-acp-work-")
    sock = str(Path(home) / "acp" / "daemon.sock")
    env = dict(os.environ)
    env.update({
        "ORCA_HOME": home,
        "ORCA_API_KEY": "acp-param-probe",
        "ORCA_BASE_URL": f"http://127.0.0.1:{port}",
    })
    env.pop("DEEPSEEK_API_KEY", None)

    checks: list[tuple[str, bool, str, str]] = []
    results: list[dict] = []

    def record(name: str, ok: bool, detail: str, issue: str = "") -> None:
        checks.append((name, ok, detail, issue))
        results.append({"case": name, "ok": ok, "detail": detail, "issue": issue})

    daemon = subprocess.Popen(
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

    bridge = Bridge(binary, sock, env, work)
    try:
        # 1. A request before initialize: must be answered, not swallowed.
        bridge.send({"jsonrpc": "2.0", "id": 1, "method": "session/new",
                     "params": {"cwd": work, "mcpServers": []}})
        reply = bridge.await_id(1, 15.0)
        record(
            "pre-initialize request answered",
            reply is not None and reply.get("id") == 1,
            describe(reply),
        )

        # 2. Unknown method: JSON-RPC -32601, id echoed.
        bridge.send({"jsonrpc": "2.0", "id": 2, "method": "session/does_not_exist", "params": {}})
        reply = bridge.await_id(2, 15.0)
        code = ((reply or {}).get("error") or {}).get("code")
        record(
            "unknown method → -32601",
            reply is not None and reply.get("id") == 2 and code == -32601,
            describe(reply),
        )

        # 3. Malformed params on known methods: -32602 + id echo. Both shapes a real
        #    client produces: the whole `params` value of the wrong JSON kind, and a
        #    single field of the wrong type (schema/version skew).
        client_info = {"name": "acp-param-probe", "version": "0.0.1"}
        malformed: list[tuple[int, str, object]] = [
            (3, "initialize", "not-an-object"),
            (4, "session/new", "not-an-object"),
            (5, "session/prompt", "not-an-object"),
            (6, "initialize", {"protocolVersion": 1, "clientCapabilities": 42, "clientInfo": client_info}),
            (7, "session/new", {"cwd": 42, "mcpServers": []}),
            (8, "session/load", {"sessionId": 42, "cwd": work, "mcpServers": []}),
            (9, "authenticate", {"methodId": 42}),
            (15, "orca.dev/session/queue/list", "not-an-object"),
            (16, "session/set_config_option", {"sessionId": 42}),
        ]
        for request_id, method, params in malformed:
            if isinstance(params, str):
                shape = "params not an object"
            else:
                bad = next(key for key, value in params.items() if key != "protocolVersion")
                shape = f"{bad} wrong type"
            bridge.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
            reply = bridge.await_id(request_id, 15.0)
            code = ((reply or {}).get("error") or {}).get("code")
            ok = reply is not None and reply.get("id") == request_id and code == -32602
            record(
                f"{method} bad params ({shape}) → -32602",
                ok,
                describe(reply),
                issue="" if ok else "blocked by #80",
            )

        # 4. Explicit null id (JSON-RPC: a request with null id is still a request).
        bridge.send({"jsonrpc": "2.0", "id": None, "method": "session/does_not_exist", "params": {}})
        reply = bridge.await_id(None, 10.0)
        record("null id request answered", reply is not None, describe(reply))

        # 5. Valid handshake (must still work after the hostile battery above),
        #    then a duplicate initialize (ACP allows it once) and unknown-session calls.
        bridge.send({
            "jsonrpc": "2.0", "id": 10, "method": "initialize",
            "params": {
                "protocolVersion": 1,
                "clientCapabilities": {"fs": {"readTextFile": False, "writeTextFile": False}},
                "clientInfo": {"name": "acp-param-probe", "version": "0.0.1"},
            },
        })
        init = bridge.await_id(10, 25.0)
        record(
            "initialize works after hostile input",
            bool(((init or {}).get("result") or {}).get("agentCapabilities")),
            describe(init),
        )

        bridge.send({"jsonrpc": "2.0", "id": 14, "method": "initialize",
                     "params": {"protocolVersion": 1, "clientCapabilities": {}, "clientInfo": client_info}})
        duplicate = bridge.await_id(14, 15.0)
        duplicate_code = ((duplicate or {}).get("error") or {}).get("code")
        record(
            "duplicate initialize → error, id echoed",
            duplicate is not None and duplicate.get("id") == 14 and duplicate_code is not None,
            describe(duplicate),
        )

        for request_id, method, params in (
            (11, "session/prompt", {"sessionId": "no-such-session", "prompt": [{"type": "text", "text": "hi"}]}),
            (12, "session/set_model", {"sessionId": "no-such-session", "modelId": "deepseek-flash"}),
            (13, "session/load", {"sessionId": "no-such-session", "cwd": work, "mcpServers": []}),
        ):
            bridge.send({"jsonrpc": "2.0", "id": request_id, "method": method, "params": params})
            reply = bridge.await_id(request_id, 25.0)
            error = (reply or {}).get("error") or {}
            ok = reply is not None and reply.get("id") == request_id and error.get("code") not in (-32603, None)
            record(
                f"{method} unknown session → non-internal error",
                ok,
                describe(reply),
            )

        # 6. Unknown-session cancel is a notification: no reply, no crash.
        bridge.send({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": "no-such-session"}})
        time.sleep(2.0)
        record("unknown session/cancel survives", bridge.alive(), f"rc={bridge.process.poll()}")

        # 7. Still usable: a real session and a real prompt.
        bridge.send({"jsonrpc": "2.0", "id": 20, "method": "session/new",
                     "params": {"cwd": work, "mcpServers": []}})
        session = bridge.await_id(20, 25.0)
        session_id = ((session or {}).get("result") or {}).get("sessionId")
        record("session/new still works", bool(session_id), describe(session))
        if session_id:
            bridge.send({"jsonrpc": "2.0", "id": 21, "method": "session/prompt",
                         "params": {"sessionId": session_id, "prompt": [{"type": "text", "text": "FIMODE=clean acp probe"}]}})
            prompt = bridge.await_id(21, 60.0)
            record(
                "session/prompt still works",
                bool(((prompt or {}).get("result") or {}).get("stopReason")),
                describe(prompt),
            )
    finally:
        bridge.close()
        daemon.terminate()
        try:
            daemon.wait(timeout=10)
        except subprocess.TimeoutExpired:
            daemon.kill()
        provider.shutdown()

    (out_dir / "checks.jsonl").write_text("\n".join(json.dumps(item) for item in results) + "\n")
    failures = 0
    for name, ok, detail, issue in checks:
        failures += 0 if ok else 1
        suffix = f" — {issue}" if issue and not ok else ""
        print(f"[{'PASS' if ok else 'FAIL'}] {name:<44} {detail[:105]}{suffix}")
    print(f"\nlogs: {out_dir}")
    print(f"verdict: {'PASS' if not failures else f'FAIL ({failures} checks)'}")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
