#!/usr/bin/env python3
"""JSONL wire-parameter contract probe.

Two contracts that the other suites do not cover, both on `orca --mode server`:

1. **Error correlation.** Every reply carries `id`. A client that correlates responses
   by `id` must still be able to match an *error* that was caused by its own request.
   Payloads whose `id` is well-formed but whose *params* are malformed must therefore
   echo that `id` back. Today the whole line is deserialized as one struct, so a single
   bad field (`"limit": "ten"`) discards the id and replies with `"id": null`.
2. **Input shorthand.** `input` accepts either a block list or a bare string
   (`WireInputParam::Text`, accepted for `thread/queue/add`). A bare string must either
   deliver the text or be rejected — it must never start a turn with an empty prompt.

The provider is local (`mock_provider.py`) and marker-matched (`MOCK_WATCH`), so the
suite asserts what the model actually received, not what the server claimed.

Usage:
    python3 scripts/eval/wire_param_probe.py [--binary target/release/orca]
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

MARKER = "WIREMARKER"
os.environ["MOCK_WATCH"] = MARKER

# `id` is correct and present in every one of these; only the params are malformed.
ID_CASES: list[tuple[str, str]] = [
    ("limit wrong type", '{"id":"c-limit","method":"thread/list","params":{"limit":"ten"}}'),
    ("params as array", '{"id":"c-params","method":"thread/start","params":[]}'),
    ("decision unknown variant", '{"id":"c-decision","method":"permission/respond","params":{"requestId":"x","decision":"maybe"}}'),
    ("threadId wrong type", '{"id":"c-thread","method":"turn/start","params":{"threadId":42,"input":[]}}'),
    ("input block missing text", '{"id":"c-block","method":"turn/start","params":{"threadId":"t","input":[{"type":"text"}]}}'),
    ("input wrong type", '{"id":"c-input","method":"turn/start","params":{"threadId":"t","input":7}}'),
    ("control: params unknown field", '{"id":"c-unknown-field","method":"thread/list","params":{"nonsense":1}}'),
]


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


class Server:
    def __init__(self, binary: str, port: int, cwd: str, env: dict) -> None:
        self.process = subprocess.Popen(
            [
                binary, "--mode", "server", "--api-key", "wire-probe",
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
        self.process.stdin.write(text + "\n")
        self.process.stdin.flush()

    def next_event(self, seconds: float = 8.0) -> dict | None:
        deadline = time.time() + seconds
        while time.time() < deadline:
            ready, _, _ = select.select([self.process.stdout], [], [], 0.5)
            if not ready:
                continue
            line = self.process.stdout.readline()
            if not line:
                return None
            line = line.strip()
            if not line.startswith("{"):
                continue
            return json.loads(line)
        return None

    def drain(self, seconds: float = 6.0, stop: tuple[str, ...] = ("turn_completed",)) -> list[dict]:
        collected: list[dict] = []
        deadline = time.time() + seconds
        while time.time() < deadline:
            ready, _, _ = select.select([self.process.stdout], [], [], 0.4)
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
            if event.get("event") in stop:
                break
        return collected

    def close(self) -> None:
        try:
            if self.process.stdin:
                self.process.stdin.close()
            self.process.wait(timeout=10)
        except (subprocess.TimeoutExpired, BrokenPipeError):
            self.process.kill()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/release/orca")
    parser.add_argument("--out", default="jobs/eval-wire-param-probe")
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    if not Path(binary).exists():
        print(f"binary not found: {binary}", file=sys.stderr)
        return 2

    stamp = time.strftime("%Y%m%d-%H%M%S")
    out_dir = Path(args.out) / stamp
    out_dir.mkdir(parents=True, exist_ok=True)
    request_log = str(out_dir / "provider-requests.jsonl")
    results: list[dict] = []
    checks: list[tuple[str, bool, str]] = []

    port = free_port()
    provider = mock_provider.serve(port, request_log, "127.0.0.1")
    tmp = tempfile.mkdtemp(prefix="orca-wire-probe-")
    env = dict(os.environ)
    env["ORCA_HOME"] = os.path.join(tmp, "home")
    os.makedirs(env["ORCA_HOME"], exist_ok=True)
    server = Server(binary, port, tmp, env)

    def record(name: str, ok: bool, detail: str, issue: str = "") -> None:
        checks.append((name, ok, detail, issue))
        results.append({"case": name, "ok": ok, "detail": detail, "issue": issue})

    try:
        # 1. Error correlation: malformed params must still echo the request id.
        for name, line in ID_CASES:
            server.send_raw(line)
            reply = server.next_event()
            sent_id = json.loads(line).get("id")
            got_id = (reply or {}).get("id", "<none>")
            ok = got_id == sent_id
            record(
                f"id echoed: {name}",
                ok,
                f"sent={sent_id!r} got={got_id!r} reply={json.dumps(reply)[:90]}",
                issue="" if ok else "blocked by #78",
            )

        # 2. Input shorthand: a bare string must not silently become an empty prompt.
        server.send_raw(json.dumps({"id": "thread", "method": "thread/start", "params": {}}))
        thread_id = next(
            (
                event.get("threadId")
                for event in server.drain(10.0, stop=("thread_started",))
                if event.get("event") == "thread_started"
            ),
            None,
        )
        record("thread/start", thread_id is not None, f"threadId={thread_id}")
        if thread_id:
            # One distinct marker per case: the thread keeps history, so a shared marker
            # would match the *previous* turn's text inside the raw request body.
            for name, build in (
                ("block list", lambda marker: [{"type": "text", "text": f"deliver {marker}"}]),
                ("bare string", lambda marker: f"deliver {marker}"),
            ):
                marker = f"{MARKER}_{name.replace(' ', '_').upper()}"
                os.environ["MOCK_WATCH"] = marker
                payload_input = build(marker)
                before = Path(request_log).stat().st_size if Path(request_log).exists() else 0
                server.send_raw(json.dumps({
                    "id": f"turn-{name}",
                    "method": "turn/start",
                    "params": {"threadId": thread_id, "input": payload_input},
                }))
                events = server.drain(30.0, stop=("turn_completed", "error"))
                seen: list[dict] = []
                if Path(request_log).exists():
                    with open(request_log) as handle:
                        handle.seek(before)
                        seen = [json.loads(line) for line in handle if line.strip()]
                marker_seen = any(entry.get("watch", {}).get(marker) for entry in seen)
                errored = any(event.get("event") == "error" for event in events)
                record(
                    f"input {name}: delivered or rejected",
                    marker_seen or errored,
                    f"marker={marker} provider_requests={len(seen)} marker_seen={marker_seen} "
                    f"error={errored} prompt_lens={[e.get('prompt_len') for e in seen]}",
                    issue="" if (marker_seen or errored) else "blocked by #77",
                )
        record("server alive", server.process.poll() is None, f"rc={server.process.poll()}")

        # 3. A bad request must not take the process down (issue #79): `thread/resume`
        #    with an unknown id used to exit the server with rc=1 and no reply at all.
        #    Runs on a throwaway instance so a fatal outcome cannot poison the checks above.
        victim = Server(binary, port, tmp, env)
        try:
            victim.send_raw(json.dumps({
                "id": "resume-unknown",
                "method": "thread/resume",
                "params": {"threadId": "no-such-thread"},
            }))
            reply = victim.next_event(10.0)
            record(
                "thread/resume unknown: error reply",
                bool(reply) and reply.get("event") == "error" and reply.get("id") == "resume-unknown",
                f"reply={json.dumps(reply)[:90]}",
                issue="" if reply else "blocked by #79",
            )
            alive = victim.process.poll() is None
            record(
                "thread/resume unknown: server survives",
                alive,
                f"rc={victim.process.poll()}",
                issue="" if alive else "blocked by #79",
            )
            if alive:
                victim.send_raw(json.dumps({"id": "after-resume", "method": "thread/start", "params": {}}))
                follow = victim.drain(10.0, stop=("thread_started", "error"))
                ok = any(event.get("event") == "thread_started" for event in follow)
                record("thread/resume unknown: still serving", ok, f"events={len(follow)}")
            else:
                record(
                    "thread/resume unknown: still serving",
                    False,
                    "server already exited",
                    issue="blocked by #79",
                )
        finally:
            victim.close()
    finally:
        server.close()
        provider.shutdown()

    (out_dir / "checks.jsonl").write_text(
        "\n".join(json.dumps(item) for item in results) + "\n"
    )
    failures = 0
    for name, ok, detail, issue in checks:
        failures += 0 if ok else 1
        suffix = f" — {issue}" if issue and not ok else ""
        print(f"[{'PASS' if ok else 'FAIL'}] {name:<42} {detail[:110]}{suffix}")
    print(f"\nlogs: {out_dir}")
    print(f"verdict: {'PASS' if not failures else f'FAIL ({failures} checks)'}")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
