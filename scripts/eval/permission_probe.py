#!/usr/bin/env python3
"""Sandbox-policy / permission probe through the server surface.

Runs `orca --mode server` inside a **privileged** Linux container (so bwrap really
enforces) and drives `command/exec` with explicit sandbox policies:

* `readOnly`            → writes are refused
* `workspaceWrite`      → writes inside the granted root succeed, escapes refused
* `dangerFullAccess`    → writes anywhere succeed (control)

This is the first suite that checks the permission *contract* rather than the
"no sandbox available" path.

Usage:
    python3 scripts/eval/permission_probe.py [--binary target/x86_64-unknown-linux-musl/release]
"""

from __future__ import annotations

import argparse
import json
import select
import socket
import subprocess
import sys
import time
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import mock_provider  # noqa: E402

IMAGE = "alpine:latest"


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


class ContainerServer:
    """`orca --mode server` running inside a privileged container, driven over stdio."""

    def __init__(self, binary_dir: Path, port: int, name: str) -> None:
        self.name = name
        subprocess.run(["docker", "rm", "-f", name], capture_output=True)
        self.container = subprocess.run(
            [
                "docker", "run", "-d", "--name", name, "--privileged",
                "--add-host=host.docker.internal:host-gateway",
                "-v", f"{binary_dir}:/mnt:ro",
                "-e", f"ORCA_BASE_URL=http://host.docker.internal:{port}",
                "-e", "ORCA_API_KEY=permission-probe",
                "-e", "ORCA_HOME=/tmp/orca-home",
                IMAGE, "sh", "-c",
                "apk add --no-cache bubblewrap >/dev/null 2>&1; mkdir -p /work /tmp/orca-home;"
                " touch /tmp/ready; sleep 3600",
            ],
            capture_output=True,
            text=True,
            check=True,
        ).stdout.strip()
        # Wait until the container can actually run a command (apk add finished).
        for _ in range(60):
            probe = subprocess.run(
                ["docker", "exec", name, "test", "-f", "/tmp/ready"],
                capture_output=True,
                text=True,
            )
            if probe.returncode == 0:
                break
            time.sleep(0.5)
        time.sleep(1.0)
        self.process = subprocess.Popen(
            [
                "docker", "exec", "-i", name, "sh", "-c",
                "cd /work && /mnt/orca --mode server --api-key permission-probe "
                f"--base-url http://host.docker.internal:{port}",
            ],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            bufsize=1,
        )

    def call(self, method: str, params: dict, seconds: float = 45.0) -> list[dict]:
        request_id = uuid.uuid4().hex[:8]
        assert self.process.stdin
        self.process.stdin.write(
            json.dumps({"id": request_id, "method": method, "params": params}) + "\n"
        )
        self.process.stdin.flush()
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
            message = json.loads(line)
            collected.append(message)
            if message.get("id") == request_id:
                break
        return collected

    def close(self) -> None:
        try:
            if self.process.stdin:
                self.process.stdin.close()
            self.process.wait(timeout=10)
        except (subprocess.TimeoutExpired, BrokenPipeError):
            self.process.kill()
        subprocess.run(["docker", "rm", "-f", self.name], capture_output=True)


def exec_case(server: ContainerServer, policy: dict, command: str) -> dict:
    events = server.call(
        "command/exec",
        {"command": ["sh", "-lc", command], "sandboxPolicy": policy, "timeoutMs": 30000},
    )
    result = {"events": len(events), "exit_code": None, "stdout": "", "stderr": "", "error": ""}
    for event in events:
        if "exitCode" in event:  # command_exec_completed
            result["exit_code"] = event.get("exitCode")
            result["stdout"] = str(event.get("stdout") or "")[:160]
            result["stderr"] = str(event.get("stderr") or "")[:160]
        elif event.get("event") == "error":
            result["error"] = str(event.get("message") or event.get("error") or "")[:200]
    return result


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/x86_64-unknown-linux-musl/release")
    parser.add_argument("--out", default="jobs/eval-permission-probe")
    parser.add_argument("--timeout", type=float, default=240.0)
    args = parser.parse_args()
    binary_dir = Path(args.binary).resolve()
    if not (binary_dir / "orca").exists():
        print(f"binary not found under {binary_dir}", file=sys.stderr)
        return 2

    port = free_port()
    server_mock = mock_provider.serve(port, f"{args.out}-requests.jsonl", "0.0.0.0")
    server = ContainerServer(binary_dir, port, f"orca-permission-probe-{uuid.uuid4().hex[:6]}")
    failures = 0
    checks: list[tuple[str, bool, str]] = []

    def session_events(stop_events: tuple[str, ...], seconds: float = 25.0) -> list[dict]:
        """Read server events until one of `stop_events` (or the deadline)."""
        collected: list[dict] = []
        deadline = time.time() + seconds
        while time.time() < deadline:
            ready, _, _ = select.select([server.process.stdout], [], [], 0.5)
            if not ready:
                continue
            line = server.process.stdout.readline()
            if not line:
                break
            line = line.strip()
            if not line.startswith("{"):
                continue
            event = json.loads(line)
            collected.append(event)
            if event.get("event") in stop_events:
                break
        return collected

    def respond_case(params: dict) -> tuple[str, str]:
        """Run one turn, answer its permission request with `params`, report the outcome."""
        server.process.stdin.write(
            json.dumps({"id": "thread", "method": "thread/start", "params": {}}) + "\n"
        )
        server.process.stdin.flush()
        thread_id = next(
            (e.get("threadId") for e in session_events(("thread_started",), 10)), None
        )
        server.process.stdin.write(
            json.dumps({
                "id": "turn",
                "method": "turn/start",
                "params": {
                    "threadId": thread_id,
                    "input": [{"type": "text", "text": "FIMODE=tool_script FITOOL=request_permissions"}],
                },
            }) + "\n"
        )
        server.process.stdin.flush()
        request = next(
            (e for e in session_events(("permission_request",), 20)
             if e.get("event") == "permission_request"),
            None,
        )
        if not request:
            return "no permission_request", ""
        server.process.stdin.write(
            json.dumps({
                "id": "resp",
                "method": "permission/respond",
                "params": {"requestId": request["requestId"], **params},
            }) + "\n"
        )
        server.process.stdin.flush()
        outcome = session_events(("permission_resolved",), 15)
        resolved = any(e.get("event") == "permission_resolved" for e in outcome)
        message = next((str(e.get("message")) for e in outcome if e.get("event") == "error"), "")
        session_events(("turn_completed",), 8)
        return ("permission_resolved" if resolved else "error"), message

    try:
        inside = exec_case(
            server, {"type": "workspaceWrite", "writableRoots": ["/work"], "networkAccess": False},
            "echo POLICY-OK > /work/policy.txt && cat /work/policy.txt",
        )
        checks.append((
            "workspaceWrite: write inside allowed",
            inside.get("exit_code") == 0 and "POLICY-OK" in inside.get("stdout", ""),
            f"exit={inside.get('exit_code')} err={inside.get('error','')[:60]}",
        ))

        escape = exec_case(
            server, {"type": "workspaceWrite", "writableRoots": ["/work"], "networkAccess": False},
            "touch /etc/policy-escape && echo ESCAPED",
        )
        checks.append((
            "workspaceWrite: escape refused",
            (escape.get("exit_code") not in (0, None) or escape.get("error"))
            and "ESCAPED" not in escape.get("stdout", ""),
            f"exit={escape.get('exit_code')} out={escape.get('stdout','')[:50]!r}",
        ))

        readonly = exec_case(server, {"type": "readOnly"}, "echo NOPE > /work/readonly.txt")
        checks.append((
            "readOnly: write refused",
            readonly.get("exit_code") not in (0, None) or bool(readonly.get("error")),
            f"exit={readonly.get('exit_code')} err={readonly.get('error','')[:60]}",
        ))

        danger = exec_case(server, {"type": "dangerFullAccess"}, "echo DANGER-OK > /etc/policy-danger && cat /etc/policy-danger")
        checks.append((
            "dangerFullAccess: unrestricted (control)",
            danger.get("exit_code") == 0 and "DANGER-OK" in danger.get("stdout", ""),
            f"exit={danger.get('exit_code')} err={danger.get('stderr','')[:40]!r}",
        ))

        # Interactive permission flow: the same request answered five ways.
        interactions = [
            ("allow/turn/empty", {"decision": "allow", "scope": "turn", "permissions": {}}, True),
            (
                "allow/turn/fileSystem+network",
                {
                    "decision": "allow",
                    "scope": "turn",
                    "permissions": {
                        "fileSystem": {"write": ["/work/granted"]},
                        "network": {"domains": {"host.docker.internal": "allow"}},
                    },
                },
                True,
            ),
            ("allow/session/empty", {"decision": "allow", "scope": "session", "permissions": {}}, True),
            (
                # Session-scoped network grants have no runtime policy to persist into, so the
                # *grant* is refused — but the request stays live, and the rejection must say
                # so instead of claiming the request expired (issue #74).
                "allow/session/network",
                {
                    "decision": "allow",
                    "scope": "session",
                    "permissions": {"network": {"domains": {"host.docker.internal": "allow"}}},
                },
                None,  # rejected with an explicit reason; asserted below
                "scope=turn",
            ),
            ("deny/turn", {"decision": "deny", "scope": "turn", "permissions": {}}, True),
        ]
        def ask_case(answer: str | None, request_id_override: str | None = None) -> tuple[str, str]:
            """Drive `ask_user_question` and answer it (or not)."""
            server.process.stdin.write(
                json.dumps({"id": "thread", "method": "thread/start", "params": {}}) + "\n"
            )
            server.process.stdin.flush()
            thread_id = next(
                (e.get("threadId") for e in session_events(("thread_started",), 10)), None
            )
            server.process.stdin.write(
                json.dumps({
                    "id": "turn",
                    "method": "turn/start",
                    "params": {
                        "threadId": thread_id,
                        "input": [{"type": "text", "text": "FIMODE=tool_script FITOOL=ask"}],
                    },
                }) + "\n"
            )
            server.process.stdin.flush()
            request = next(
                (e for e in session_events(("user_input_request",), 20)
                 if e.get("event") == "user_input_request"),
                None,
            )
            if not request:
                return "no user_input_request", ""
            request_id = request_id_override or request.get("requestId") or (
                (request.get("questions") or [{}])[0].get("id")
            )
            server.process.stdin.write(
                json.dumps({
                    "id": "answer",
                    "method": "user_input/respond",
                    "params": {"requestId": request_id, "answer": answer},
                }) + "\n"
            )
            server.process.stdin.flush()
            outcome = session_events(("user_input_resolved",), 15)
            resolved = any(e.get("event") == "user_input_resolved" for e in outcome)
            message = next((str(e.get("message")) for e in outcome if e.get("event") == "error"), "")
            session_events(("turn_completed",), 10)
            return ("user_input_resolved" if resolved else "error"), message

        choice_outcome, _ = ask_case("main")
        checks.append((
            "user input: listed choice",
            choice_outcome == "user_input_resolved",
            choice_outcome,
        ))
        free_outcome, _ = ask_case("custom answer text")
        checks.append((
            "user input: free text",
            free_outcome == "user_input_resolved",
            free_outcome,
        ))
        unknown_outcome, unknown_message = ask_case(None, request_id_override="does-not-exist")
        checks.append((
            "user input: unknown request id",
            unknown_outcome == "error" and "unknown user input request" in unknown_message,
            f"{unknown_outcome} {unknown_message[:50]}",
        ))

        def replay_case() -> list[tuple[str, str]]:
            """Answer one request, then replay it, then send a conflicting one."""
            server.process.stdin.write(
                json.dumps({"id": "thread", "method": "thread/start", "params": {}}) + "\n"
            )
            server.process.stdin.flush()
            thread_id = next(
                (e.get("threadId") for e in session_events(("thread_started",), 10)), None
            )
            server.process.stdin.write(
                json.dumps({
                    "id": "turn",
                    "method": "turn/start",
                    "params": {
                        "threadId": thread_id,
                        "input": [{"type": "text", "text": "FIMODE=tool_script FITOOL=request_permissions"}],
                    },
                }) + "\n"
            )
            server.process.stdin.flush()
            request = next(
                (e for e in session_events(("permission_request",), 20)
                 if e.get("event") == "permission_request"),
                None,
            )
            responses = []
            if not request:
                return [("no request", "")]
            allow = {"requestId": request["requestId"], "decision": "allow", "scope": "turn", "permissions": {}}
            for label, params in (
                ("first", allow),
                ("replay", allow),
                ("conflict", {**allow, "decision": "deny"}),
                ("unknown", {**allow, "requestId": "never-existed"}),
            ):
                server.process.stdin.write(
                    json.dumps({"id": label, "method": "permission/respond", "params": params}) + "\n"
                )
                server.process.stdin.flush()
                events = session_events(("permission_resolved",), 10)
                if not any(e.get("event") == "permission_resolved" for e in events):
                    events += session_events(("error",), 5)
                event = next(
                    (e for e in events if e.get("event") in ("permission_resolved", "error")), None
                )
                responses.append(
                    (str((event or {}).get("event")), str((event or {}).get("message") or ""))
                )
            session_events(("turn_completed",), 10)
            return responses

        replay = replay_case()
        labels = ["first", "replay", "conflict", "unknown"]
        outcomes = {label: (kind, message) for label, (kind, message) in zip(labels, replay)}
        checks.append((
            "replay: first response resolves",
            outcomes.get("first", ("", ""))[0] == "permission_resolved",
            outcomes.get("first", ("", ""))[0],
        ))
        checks.append((
            "replay: identical response is idempotent",
            outcomes.get("replay", ("", ""))[0] == "permission_resolved",
            outcomes.get("replay", ("", ""))[0],
        ))
        conflict = outcomes.get("conflict", ("", ""))
        checks.append((
            "replay: conflicting response rejected",
            conflict[0] == "error" and "already resolved with a different response" in conflict[1],
            f"{conflict[0]} {conflict[1][:60]}",
        ))
        unknown = outcomes.get("unknown", ("", ""))
        checks.append((
            "replay: unknown request id rejected",
            unknown[0] == "error" and "unknown permission request" in unknown[1],
            f"{unknown[0]} {unknown[1][:60]}",
        ))

        for case in interactions:
            label, params, should_resolve = case[0], case[1], case[2]
            expected_hint = case[3] if len(case) > 3 else None
            outcome, message = respond_case(params)
            if expected_hint is not None:
                # A refused *grant* must name the reason and must not claim the request expired
                # (issue #74); the request itself stays live.
                ok = (
                    outcome == "error"
                    and expected_hint in message
                    and "no longer active" not in message
                )
            else:
                ok = (outcome == "permission_resolved") == should_resolve
            note = f" [{message[:70]}]" if (not ok or expected_hint is not None) else ""
            checks.append((f"interaction {label}", ok, f"{outcome}{note}"))
    finally:
        server.close()
        server_mock.shutdown()

    for name, ok, detail in checks:
        failures += 0 if ok else 1
        note = ""
        if not ok and "127" in detail and "applet not found" in detail:
            note = "  (blocked by #70: BusyBox shell canonicalised on the unsandboxed path)"
        print(f"[{'PASS' if ok else 'FAIL'}] {name:<38} {detail}{note}", flush=True)
    print(f"\nverdict: {'PASS' if not failures else 'FAIL'}")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
