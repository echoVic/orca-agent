#!/usr/bin/env python3
"""Linux container probe for the musl binary (L1, Linux runtime).

Checks two contracts separately (issue #127):

1. Shell: BusyBox vs dash. The shell resolver canonicalises `/bin/sh`, which on
   BusyBox systems is a symlink to the multicall binary. Running
   `/bin/busybox -c '<script>'` then fails with `-c: applet not found` (exit 127)
   and *no* command can run on Alpine-style images, while Debian-style images
   work. One trivial bash-tool command runs through the musl binary in both
   images and must complete. Orca builds no HTTP client without CA certificates,
   even for this probe's plain-http provider, so both images get the host's CA
   bundle through `SSL_CERT_FILE`, the way the terminal-bench adapter lends one
   to task images that have none.

2. CA bundle: Orca has no root certificates compiled in and reads the system
   trust store only. On an image without a CA bundle (`debian:bookworm-slim`
   ships none) the run must fail with the documented error, `No CA certificates
   were loaded from the system; install the ca-certificates package`, without
   sending a request.

Usage:
    python3 scripts/eval/linux_shell_probe.py [--binary target/x86_64-unknown-linux-musl/release]
"""

from __future__ import annotations

import argparse
import json
import shutil
import socket
import ssl
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import mock_provider  # noqa: E402

IMAGES = ["alpine:latest", "debian:bookworm-slim"]
PROMPT = "FIMODE=tool_script FITOOL=bash FISLEEP=2"
#: An image without a CA bundle, for the CA check.
BARE_IMAGE = "debian:bookworm-slim"
#: The documented error for a system without CA certificates (v0.5.8).
NO_CA_ERROR = (
    "No CA certificates were loaded from the system",
    "install the ca-certificates package",
)
#: Where the host's CA bundle is mounted for the shell check.
CA_MOUNT = "/orca-probe-ca"


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def host_ca_bundle() -> Path | None:
    for candidate in (
        ssl.get_default_verify_paths().cafile,
        "/etc/ssl/certs/ca-certificates.crt",
        "/etc/ssl/cert.pem",
    ):
        if candidate and Path(candidate).is_file() and Path(candidate).stat().st_size:
            return Path(candidate)
    return None


def run_in_image(
    image: str, binary_dir: Path, port: int, timeout: float, ca_dir: Path | None
) -> subprocess.CompletedProcess:
    command = (
        "mkdir -p /work /tmp/orca-home; cd /work; "
        "/mnt/orca exec --mode full-auto --output-format jsonl --no-history -- "
        f"'{PROMPT}'"
    )
    docker = [
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
    ]
    if ca_dir is not None:
        docker += [
            "-v",
            f"{ca_dir}:{CA_MOUNT}:ro",
            "-e",
            f"SSL_CERT_FILE={CA_MOUNT}/cacert.pem",
        ]
    return subprocess.run(
        [*docker, image, "sh", "-c", command],
        capture_output=True,
        text=True,
        timeout=timeout,
    )


def tool_outcome(result: subprocess.CompletedProcess) -> dict:
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
    bundle = host_ca_bundle()
    if bundle is None:
        print("no CA bundle found on the host to lend the images", file=sys.stderr)
        return 2

    port = free_port()
    server = mock_provider.serve(port, "/tmp/linux-shell-probe-requests.jsonl", "0.0.0.0")
    requests = server.RequestHandlerClass.state
    outcomes = {}
    try:
        print("shell contract (CA bundle lent through SSL_CERT_FILE):")
        with tempfile.TemporaryDirectory() as ca_dir:
            shutil.copyfile(bundle, Path(ca_dir) / "cacert.pem")
            for image in IMAGES:
                outcome = tool_outcome(
                    run_in_image(image, binary_dir, port, args.timeout, Path(ca_dir))
                )
                outcomes[image] = outcome
                print(
                    f"  {image:<24} state={outcome['state']!s:<10} exit={outcome['exit_code']!s:<5} "
                    f"{outcome['output'][:70]!r}",
                    flush=True,
                )
        sent_before = requests.seq
        bare = run_in_image(BARE_IMAGE, binary_dir, port, args.timeout, None)
        sent = requests.seq - sent_before
    finally:
        server.shutdown()

    shell_ok = all(
        o.get("state") == "completed" and o.get("exit_code") == 0 for o in outcomes.values()
    )
    reported = all(part in bare.stdout + bare.stderr for part in NO_CA_ERROR)
    ca_ok = bare.returncode == 1 and reported and sent == 0
    print("CA contract (no CA bundle):")
    print(
        f"  {BARE_IMAGE:<24} exit={bare.returncode!s:<3} requests={sent} "
        f"documented_error={'yes' if reported else 'no'}",
        flush=True,
    )

    if not shell_ok and outcomes.get("alpine:latest", {}).get("exit_code") == 127:
        print(
            "\nverdict: FAIL — BusyBox shell resolved to the multicall binary"
            " (`-c: applet not found`) (known issue #70)"
        )
    else:
        print(
            "\nverdict:",
            "PASS" if shell_ok and ca_ok else "FAIL",
            f"(shell {'ok' if shell_ok else 'FAIL'}, CA {'ok' if ca_ok else 'FAIL'})",
        )
    return 0 if shell_ok and ca_ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
