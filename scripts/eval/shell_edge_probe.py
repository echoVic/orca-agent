#!/usr/bin/env python3
"""Shell edge-case probe (L1): output encoding, signals and exit codes.

Drives `orca exec` against scripts/eval/mock_provider.py in `cmd` mode and checks
that the bash tool reports what really happened for awkward commands: binary
output, one very long line, signals (TERM/SEGV), exit 255, missing trailing
newline, invalid UTF-8, and empty output.

Usage:
    python3 scripts/eval/shell_edge_probe.py [--binary target/release/orca]
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import shlex
import socket
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import mock_provider  # noqa: E402

CASES = [
    ("binary_output", "head -c 200 /dev/urandom", {"state": "completed"}),
    ("huge_single_line", "head -c 300000 /dev/zero | tr '\\0' 'a'", {"state": "running"}),
    ("signal_term", "kill -TERM $$", {"state": "failed", "exit_code": 143}),
    ("signal_segv", "kill -SEGV $$", {"state": "failed", "exit_code": 139}),
    ("exit_255", "exit 255", {"state": "failed", "exit_code": 255}),
    ("no_trailing_newline", "printf 'no-newline'", {"state": "completed"}),
    ("invalid_utf8_text", "printf '\\xff\\xfe bad bytes\\n'", {"state": "completed"}),
    ("empty_output", "true", {"state": "completed"}),
]


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/release/orca")
    parser.add_argument("--out", default="jobs/eval-shell-edge")
    parser.add_argument("--timeout", type=float, default=120.0)
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    if not Path(binary).exists():
        print(f"binary not found: {binary}", file=sys.stderr)
        return 2

    port = free_port()
    server = mock_provider.serve(port, f"{args.out}-requests.jsonl")
    failures = 0
    try:
        for name, command, expected in CASES:
            token = base64.b64encode(command.encode()).decode()
            prompt = f"FIMODE=tool_script FITOOL=cmd FICMD64={token}"
            env = dict(os.environ)
            env["PATH"] = f"{Path(binary).parent}{os.pathsep}{env.get('PATH','')}"
            env.pop("DEEPSEEK_API_KEY", None)
            with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as cwd:
                env["ORCA_HOME"] = home
                line = (
                    f"orca exec --mode full-auto --output-format jsonl"
                    f" --base-url http://127.0.0.1:{port} --api-key edge"
                    f" --cwd {shlex.quote(cwd)} --no-history -- {shlex.quote(prompt)}"
                )
                result = subprocess.run(
                    ["/bin/sh", "-c", line],
                    capture_output=True,
                    text=True,
                    timeout=args.timeout,
                    env=env,
                )
            detail: dict = {}
            for line in result.stdout.splitlines():
                if '"tool.call.completed"' not in line:
                    continue
                payload = json.loads(line)["payload"]
                # Failed calls carry the structured result in `error`, not `output`.
                raw = payload.get("output") or payload.get("error") or ""
                try:
                    detail = json.loads(raw)
                except json.JSONDecodeError:
                    detail = {}
            ok = all(detail.get(key) == value for key, value in expected.items())
            failures += 0 if ok else 1
            note = "" if ok else f"  expected {expected}"
            print(
                f"[{'PASS' if ok else 'FAIL'}] {name:<20} state={detail.get('state')!s:<10} "
                f"exit={detail.get('exit_code')!s:<6} bytes={len(detail.get('output') or '')}{note}",
                flush=True,
            )
    finally:
        server.shutdown()
    print(f"\nverdict: {'PASS' if not failures else 'FAIL'}")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
