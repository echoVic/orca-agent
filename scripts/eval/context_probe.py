#!/usr/bin/env python3
"""Long-context probe: does a session survive filling its context window?

Drives `orca exec` against scripts/eval/mock_provider.py in `filler` mode: every
turn runs a command that returns a large page of output, so the context grows
until the runtime compacts it. Reports the observed context growth, compaction
events, and whether the session still completes with a usable transcript.

Usage:
    python3 scripts/eval/context_probe.py [--binary target/release/orca] [--turns 40]
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


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/release/orca")
    parser.add_argument("--out", default="jobs/eval-context-probe")
    parser.add_argument("--turns", type=int, default=40)
    parser.add_argument("--timeout", type=float, default=900.0)
    args = parser.parse_args()

    binary = str(Path(args.binary).resolve())
    if not Path(binary).exists():
        print(f"binary not found: {binary}", file=sys.stderr)
        return 2

    stamp = time.strftime("%Y%m%d-%H%M%S")
    out_dir = Path(args.out) / stamp
    out_dir.mkdir(parents=True, exist_ok=True)
    port = free_port()
    requests = out_dir / "requests.jsonl"
    server = mock_provider.serve(port, str(requests))

    # Watch the original instruction across compaction: the mock records whether
    # each request body still contains this marker.
    marker = f"ORCA-CONTEXT-MARKER-{stamp}"
    os.environ["MOCK_WATCH"] = marker
    prompt = (
        f"{marker} Keep inspecting the listing. FIMODE=tool_script FITOOL=filler"
        f" FIFILL={args.turns}"
    )
    started = time.time()
    try:
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as cwd:
            command = (
                f"orca exec --mode full-auto --output-format jsonl"
                f" --base-url http://127.0.0.1:{port} --api-key context-probe"
                f" --cwd {shlex.quote(cwd)} --no-history -- {shlex.quote(prompt)}"
            )
            env = dict(os.environ)
            env["ORCA_HOME"] = home
            env["PATH"] = f"{Path(binary).parent}{os.pathsep}{env.get('PATH','')}"
            env.pop("DEEPSEEK_API_KEY", None)
            completed = subprocess.run(
                ["/bin/sh", "-c", command],
                capture_output=True,
                text=True,
                timeout=args.timeout,
                env=env,
            )
            stdout, code = completed.stdout, completed.returncode
    except subprocess.TimeoutExpired as expired:
        stdout = (expired.stdout or "").decode() if isinstance(expired.stdout, bytes) else (
            expired.stdout or ""
        )
        code = None
    finally:
        server.shutdown()

    (out_dir / "session.jsonl").write_text(stdout, encoding="utf-8")
    used: list[int] = []
    limits: list[int] = []
    compactions: list[dict] = []
    status = None
    turns = tool_calls = 0
    for line in stdout.splitlines():
        if not line.startswith("{"):
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        kind = event.get("type")
        payload = event.get("payload") or {}
        if kind == "context.updated":
            if isinstance(payload.get("used_tokens"), int):
                used.append(payload["used_tokens"])
            if isinstance(payload.get("limit_tokens"), int):
                limits.append(payload["limit_tokens"])
        elif kind == "context.compaction.started":
            compactions.append(payload)
        elif kind == "session.completed":
            status = payload.get("status")
        elif kind == "turn.started":
            turns += 1
        elif kind == "tool.call.requested":
            tool_calls += 1

    delivered = []
    if requests.exists():
        for line in requests.read_text(encoding="utf-8").splitlines():
            if not line.strip():
                continue
            entry = json.loads(line)
            watch = entry.get("watch") or {}
            if marker in watch:
                delivered.append(bool(watch[marker]))
    instruction_missing = [i for i, present in enumerate(delivered) if not present]

    limit = limits[-1] if limits else None
    peak = max(used) if used else None
    report = {
        "binary": binary,
        "turns_requested": args.turns,
        "turns": turns,
        "tool_calls": tool_calls,
        "limit_tokens": limit,
        "peak_used_tokens": peak,
        "peak_share": round(peak / limit, 3) if peak and limit else None,
        "compactions": len(compactions),
        "compaction_triggers": [c.get("trigger") for c in compactions],
        "status": status,
        "exit_code": code,
        "wall_s": round(time.time() - started, 1),
        "session_path": str(out_dir / "session.jsonl"),
        "requests": len(delivered),
        "requests_missing_instruction": len(instruction_missing),
    }
    (out_dir / "report.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(json.dumps(report, indent=2))
    ok = status == "success" and code == 0 and not instruction_missing
    if instruction_missing:
        print(
            f"\ninstruction marker missing from {len(instruction_missing)} of "
            f"{len(delivered)} requests — compaction dropped the task statement"
        )
    print("\nverdict:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
