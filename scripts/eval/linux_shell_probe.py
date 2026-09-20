#!/usr/bin/env python3
"""Cross-platform shell probe: BusyBox vs dash (L1, Linux runtime).

Issue: the shell resolver canonicalises `/bin/sh`, which on BusyBox systems is a
symlink to the multicall binary. Running `/bin/busybox -c '<script>'` then fails
with `-c: applet not found` (exit 127) and *no* command can run on Alpine-style
images, while Debian-style images work.

Runs one trivial bash-tool command through the musl binary in both images and
requires both to complete.

Usage:
    python3 scripts/eval/linux_shell_probe.py [--binary target/x86_64-unknown-linux-musl/release]
"""

from __future__ import annotations

import argparse
import json
import socket
import subprocess
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import mock_provider  # noqa: E402

IMAGES = ["alpine:latest", "debian:bookworm-slim"]
PROMPT = "FIMODE=tool_script FITOOL=bash FISLEEP=2"


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def run_in_image(image: str, binary_dir: Path, port: int, timeout: float) -> dict:
    command = (
        "mkdir -p /work /tmp/orca-home; cd /work; "
        "/mnt/orca exec --mode full-auto --output-format jsonl --no-history -- "
        f"'{PROMPT}'"
    )
    result = subprocess.run(
        [
            "docker",
            "run",
            "--rm",
            "--add-host=host.docker.internal:host-gateway",
            "-v",
            f"{binary_dir}:/mnt:ro",
            "-e",
            f"ORCA_BASE_URL=http://host.docker.internal:{port}",
            "-e",
            "ORCA_API_KEY=probe",
            "-e",
            "ORCA_HOME=/tmp/orca-home",
            image,
            "sh",
            "-c",
            command,
        ],
        capture_output=True,
        text=True,
        timeout=timeout,
    )
    for line in result.stdout.splitlines():
        if '"tool.call.completed"' not in line:
            continue
        try:
            payload = json.loads(line)["payload"]
        except (json.JSONDecodeError, KeyError):
            continue
        raw = payload.get("output") or payload.get("error") or ""
        try:
            detail = json.loads(raw)
        except (json.JSONDecodeError, TypeError):
            detail = {}
        return {
            "state": detail.get("state"),
            "exit_code": detail.get("exit_code"),
            "output": str(detail.get("output") or "")[:120],
        }
    return {"state": None, "exit_code": None, "output": result.stderr[-160:]}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/x86_64-unknown-linux-musl/release")
    parser.add_argument("--timeout", type=float, default=300.0)
    args = parser.parse_args()
    binary_dir = Path(args.binary).resolve()
    if not (binary_dir / "orca").exists():
        print(f"binary not found under {binary_dir}", file=sys.stderr)
        return 2

    port = free_port()
    server = mock_provider.serve(port, "/tmp/linux-shell-probe-requests.jsonl", "0.0.0.0")
    outcomes = {}
    try:
        for image in IMAGES:
            outcome = run_in_image(image, binary_dir, port, args.timeout)
            outcomes[image] = outcome
            print(
                f"{image:<24} state={outcome['state']!s:<10} exit={outcome['exit_code']!s:<5} "
                f"{outcome['output'][:70]!r}",
                flush=True,
            )
    finally:
        server.shutdown()

    ok = all(o.get("state") == "completed" and o.get("exit_code") == 0 for o in outcomes.values())
    if not ok and outcomes.get("alpine:latest", {}).get("exit_code") == 127:
        print(
            "\nverdict: FAIL — BusyBox shell resolved to the multicall binary"
            " (`-c: applet not found`) (known issue #70)"
        )
    else:
        print("\nverdict:", "PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
