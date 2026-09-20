#!/usr/bin/env python3
"""Sandbox enforcement probe (Linux, privileged container).

With `--privileged` the container can create the namespaces bwrap needs, so this is
the one suite that exercises Orca's *real* OS-enforced sandbox instead of its
"unavailable" path:

* a trusted workspace must be writable from inside the sandbox,
* an escape (write to /etc) must be denied,
* an *untrusted* workspace currently goes read-only without saying why — the
  probe asserts that a trust/readiness warning reaches the headless stream
  (issue #73).

Usage:
    python3 scripts/eval/sandbox_enforcement_probe.py [--binary target/x86_64-unknown-linux-musl/release]
"""

from __future__ import annotations

import argparse
import base64
import json
import socket
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import mock_provider  # noqa: E402

IMAGE = "alpine:latest"
WRITE_INSIDE = "echo SANDBOX-INSIDE-OK > /work/inside.txt && cat /work/inside.txt"
WRITE_OUTSIDE = "touch /etc/orca-escape-test && echo ESCAPED"


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def run_case(binary_dir: Path, port: int, command: str, trust: bool, timeout: float) -> dict:
    token = base64.b64encode(command.encode()).decode()
    prompt = f"FIMODE=tool_script FITOOL=cmd FICMD64={token}"
    prefix = (
        "mkdir -p /tmp/orca-home; /mnt/orca trust add --cwd /work >/dev/null 2>&1; "
        if trust
        else ""
    )
    script = (
        "apk add --no-cache bubblewrap >/dev/null 2>&1; "
        "mkdir -p /work /tmp/orca-home; "
        + prefix
        + "cd /work; /mnt/orca exec --mode auto-edit --output-format jsonl --no-history -- "
        + f"'{prompt}'"
    )
    result = subprocess.run(
        [
            "docker", "run", "--rm", "--privileged",
            "--add-host=host.docker.internal:host-gateway",
            "-v", f"{binary_dir}:/mnt:ro",
            "-e", f"ORCA_BASE_URL=http://host.docker.internal:{port}",
            "-e", "ORCA_API_KEY=sandbox-probe",
            "-e", "ORCA_HOME=/tmp/orca-home",
            IMAGE, "sh", "-c", script,
        ],
        capture_output=True,
        text=True,
        timeout=timeout,
    )
    # A run can contain more than one completed tool call (retries, follow-up calls) and
    # the *last* one is not necessarily the command under test — under load that made this
    # check flaky, so take the first completed call that carries output.
    details: list[dict] = []
    for line in result.stdout.splitlines():
        if '"tool.call.completed"' not in line:
            continue
        try:
            payload = json.loads(line)["payload"]
        except (json.JSONDecodeError, KeyError, TypeError):
            continue
        raw = payload.get("output") or payload.get("error") or ""
        if isinstance(raw, str) and raw.strip().startswith("{"):
            try:
                parsed = json.loads(raw)
            except json.JSONDecodeError:
                parsed = {"raw": raw}
        else:
            parsed = {"raw": raw}
        parsed.setdefault("_tool", payload.get("name"))
        details.append(parsed)
    detail = next((item for item in details if item.get("output")), details[0] if details else {})
    lowered = result.stdout.lower()
    detail["_mentions_trust"] = "trust" in lowered or "read-only" in lowered and "warning" in lowered
    return detail


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/x86_64-unknown-linux-musl/release")
    parser.add_argument("--out", default="jobs/eval-sandbox-enforcement")
    parser.add_argument("--timeout", type=float, default=240.0)
    args = parser.parse_args()
    binary_dir = Path(args.binary).resolve()
    if not (binary_dir / "orca").exists():
        print(f"binary not found under {binary_dir}", file=sys.stderr)
        return 2

    port = free_port()
    server = mock_provider.serve(port, f"{args.out}-requests.jsonl", "0.0.0.0")
    failures = 0
    checks: list[tuple[str, bool, str]] = []
    try:
        inside = run_case(binary_dir, port, WRITE_INSIDE, trust=True, timeout=args.timeout)
        checks.append((
            "trusted workspace writable",
            inside.get("state") == "completed" and "SANDBOX-INSIDE-OK" in str(inside.get("output")),
            f"state={inside.get('state')} out={str(inside.get('output'))[:40]!r}",
        ))

        escape = run_case(binary_dir, port, WRITE_OUTSIDE, trust=True, timeout=args.timeout)
        checks.append((
            "escape write denied by sandbox",
            escape.get("state") == "failed" and "ESCAPED" not in str(escape.get("output")),
            f"state={escape.get('state')} out={str(escape.get('output'))[:50]!r}",
        ))

        untrusted = run_case(binary_dir, port, WRITE_INSIDE, trust=False, timeout=args.timeout)
        warned = untrusted.get("_mentions_trust", False)
        checks.append((
            "untrusted workspace explains itself",
            warned,
            f"state={untrusted.get('state')} warning_in_stream={warned}",
        ))
    finally:
        server.shutdown()

    for name, ok, detail in checks:
        failures += 0 if ok else 1
        note = ""
        if not ok and name.startswith("untrusted"):
            note = " (known issue #73)"
        print(f"[{'PASS' if ok else 'FAIL'}] {name:<32} {detail}{note}", flush=True)
    print(f"\nverdict: {'PASS' if not failures else 'FAIL'}")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
