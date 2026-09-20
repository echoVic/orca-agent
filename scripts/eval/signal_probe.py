#!/usr/bin/env python3
"""Signal-handling probe (L1): cancellation must clean up after itself.

Sends SIGINT and SIGTERM to `orca exec` while it runs a long command and checks
that (a) a terminal event is emitted and (b) the child command does not survive the
agent. Both currently fail — see issue: signals kill the process outright, leaving
orphans and an unterminated session.

Usage:
    python3 scripts/eval/signal_probe.py [--binary target/release/orca]
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

MARKER = "sleep 90"
PROMPT = f"FIMODE=tool_script FITOOL=bash FISLEEP=90"


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def surviving_children() -> list[str]:
    out = subprocess.run(["pgrep", "-fl", MARKER], capture_output=True, text=True).stdout
    return [line for line in out.splitlines() if line.strip()]


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/release/orca")
    parser.add_argument("--out", default="jobs/eval-signal-probe")
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    if not Path(binary).exists():
        print(f"binary not found: {binary}", file=sys.stderr)
        return 2

    port = free_port()
    server = mock_provider.serve(port, f"{args.out}-requests.jsonl")
    failures = 0
    try:
        for name, sig in (("SIGINT", signal.SIGINT), ("SIGTERM", signal.SIGTERM)):
            home, work = tempfile.mkdtemp(), tempfile.mkdtemp()
            env = dict(os.environ)
            env["ORCA_HOME"] = home
            env.pop("DEEPSEEK_API_KEY", None)
            process = subprocess.Popen(
                [
                    binary, "exec", "--mode", "full-auto", "--output-format", "jsonl",
                    "--base-url", f"http://127.0.0.1:{port}", "--api-key", "signal-probe",
                    "--cwd", work, "--no-history", "--", PROMPT,
                ],
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                env=env,
            )
            time.sleep(6)
            process.send_signal(sig)
            try:
                stdout, _ = process.communicate(timeout=20)
            except subprocess.TimeoutExpired:
                process.kill()
                stdout, _ = process.communicate()
            time.sleep(1.5)
            orphans = surviving_children()
            for orphan in orphans:
                subprocess.run(["kill", "-9", orphan.split()[0]], capture_output=True)
            terminal = any('"session.completed"' in line for line in stdout.splitlines())
            ok = terminal and not orphans
            failures += 0 if ok else 1
            print(
                f"[{'PASS' if ok else 'FAIL'}] {name:<8} exit={process.returncode!s:<5} "
                f"terminal_event={terminal!s:<5} orphans={len(orphans)}"
                + ("" if ok else " (known issue #72)"),
                flush=True,
            )
    finally:
        server.shutdown()
    print(f"\nverdict: {'PASS' if not failures else 'FAIL'}")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
