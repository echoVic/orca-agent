#!/usr/bin/env python3
"""Workspace-lifetime probe: a service the agent asked to keep must outlive the run.

`TaskLifetime::Workspace` is documented as "a long-lived service the user explicitly
asked to keep running. It survives the task that started it and is stopped
explicitly" (`crates/orca-core/src/task_types.rs`). TB2 `install-windows-3-11` started
QEMU with `lifetime: "workspace"`, the session ended with success, and the verifier
then found no QEMU process and no VNC listener, so this probe checks the contract
directly: start a ticking service with `lifetime=workspace`, end the session, and see
whether the service is still alive and still writing afterwards.

Usage:
    python3 scripts/eval/workspace_lifetime_probe.py [--binary target/release/orca]
"""

from __future__ import annotations

import argparse
import base64
import os
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import mock_provider  # noqa: E402

# 40 one-second ticks: long enough to observe after the session ends.
SERVICE = "i=0; while [ $i -lt 40 ]; do echo tick-$i >> {marker}; i=$((i+1)); sleep 1; done; echo finished"


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def tick_count(marker: Path) -> int:
    try:
        return len(marker.read_text().splitlines())
    except OSError:
        return 0


def run_case(binary: str, port: int, lifetime: str | None) -> dict:
    work = tempfile.mkdtemp()
    marker = Path(work) / "ticks"
    env = dict(os.environ)
    env["ORCA_HOME"] = tempfile.mkdtemp()
    env.pop("DEEPSEEK_API_KEY", None)
    command = SERVICE.format(marker=marker)
    tokens = [
        "FIMODE=tool_script",
        "FITOOL=bash",
        "FIYIELD=500",
        f"FICMD64={base64.b64encode(command.encode()).decode()}",
    ]
    if lifetime:
        tokens.append(f"FILIFETIME={lifetime}")
    prompt = " ".join(tokens)

    proc = subprocess.Popen(
        [
            binary, "exec", "--mode", "full-auto", "--output-format", "jsonl",
            "--base-url", f"http://127.0.0.1:{port}", "--api-key", "ws-lifetime",
            "--cwd", work, "--no-history", "--", prompt,
        ],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env,
    )
    try:
        stdout, stderr = proc.communicate(timeout=120)
    except subprocess.TimeoutExpired:
        proc.kill()
        stdout, stderr = proc.communicate()

    ticks_at_exit = tick_count(marker)
    time.sleep(4)
    ticks_after = tick_count(marker)
    survived = ticks_after > ticks_at_exit

    # Clean up whatever this case left behind.
    subprocess.run(["pkill", "-f", str(marker)], capture_output=True)
    return {
        "lifetime": lifetime or "task",
        "exit": proc.returncode,
        "ticks_at_exit": ticks_at_exit,
        "ticks_after_exit": ticks_after,
        "survived": survived,
        "terminal": any('"session.completed"' in line for line in stdout.splitlines()),
        "stderr": stderr[-200:],
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/release/orca")
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    if not Path(binary).exists():
        print(f"binary not found: {binary}", file=sys.stderr)
        return 2

    port = free_port()
    server = mock_provider.serve(port, str(Path(tempfile.mkdtemp()) / "requests.jsonl"))
    failures = 0
    try:
        for lifetime in ("workspace", None):
            result = run_case(binary, port, lifetime)
            if lifetime == "workspace":
                # Documented contract: an explicitly workspace-owned service outlives
                # the run that started it.
                ok = result["survived"]
                expected = "service keeps running after the session ends"
            else:
                # Control: a task-owned command is stopped with the session.
                ok = not result["survived"]
                expected = "task-owned command is stopped with the session"
            failures += 0 if ok else 1
            print(
                f"[{'PASS' if ok else 'FAIL'}] lifetime={result['lifetime']:<9} "
                f"exit={result['exit']} terminal={result['terminal']} "
                f"ticks_at_exit={result['ticks_at_exit']} ticks_after={result['ticks_after_exit']} "
                f"— {expected}",
                flush=True,
            )
            if not ok and result["stderr"]:
                print(f"        stderr: {result['stderr']!r}", flush=True)
    finally:
        server.shutdown()
    print(f"\nverdict: {'PASS' if not failures else 'FAIL'}")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
