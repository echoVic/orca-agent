#!/usr/bin/env python3
"""Sandbox-backend probe across working directories (L1, Linux backend).

`orca doctor` must report the *same* sandbox diagnosis regardless of where it is
run from. Issue #69: with `cwd = /` the bwrap binary is rejected by the
workspace-path guard before it is ever probed, so a container with bubblewrap
installed is told the backend is "missing".

Runs the musl binary in a throwaway container that has bubblewrap installed, once
from `/` and once from a normal directory, and compares the diagnosis.

Usage:
    python3 scripts/eval/sandbox_probe.py [--binary target/x86_64-unknown-linux-musl/release]
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path

IMAGE = "alpine:latest"


def doctor_from(
    cwd: str, binary_dir: Path, install_bwrap: bool = True, privileged: bool = False
) -> dict:
    install = "apk add --no-cache bubblewrap >/dev/null 2>&1; " if install_bwrap else ""
    command = (
        f"{install}mkdir -p /work; cd {cwd}; "
        f"/mnt/orca doctor --format json 2>/dev/null"
    )
    result = subprocess.run(
        [
            "docker",
            "run",
            "--rm",
            *(["--privileged"] if privileged else []),
            "-v",
            f"{binary_dir}:/mnt:ro",
            IMAGE,
            "sh",
            "-c",
            command,
        ],
        capture_output=True,
        text=True,
        timeout=300,
    )
    try:
        payload = json.loads(result.stdout)
    except json.JSONDecodeError:
        return {"error": "doctor did not emit JSON", "stderr": result.stderr[-300:]}
    for check in payload.get("checks", []):
        if check.get("id") == "sandbox":
            return check
    return {"error": "no sandbox check in doctor output"}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/x86_64-unknown-linux-musl/release")
    parser.add_argument(
        "--privileged",
        action="store_true",
        help="run the container privileged so bwrap can create namespaces (control case)",
    )
    args = parser.parse_args()
    binary_dir = Path(args.binary).resolve()
    if not (binary_dir / "orca").exists():
        print(f"binary not found under {binary_dir}", file=sys.stderr)
        return 2

    root = doctor_from("/", binary_dir, privileged=args.privileged)
    work = doctor_from("/work", binary_dir, privileged=args.privileged)
    root_detail = str(root.get("detail") or root.get("error"))
    work_detail = str(work.get("detail") or work.get("error"))

    print(f"cwd=/      → status={root.get('status')}: {root_detail[:160]}")
    print(f"cwd=/work  → status={work.get('status')}: {work_detail[:160]}")

    # The bug: with bwrap installed, cwd=/ claims the backend is *missing* while
    # cwd=/work actually executes it. A probe that never runs is the failure.
    missing_at_root = "backend is missing at bwrap" in root_detail
    probed_at_work = "bwrap" in work_detail and "missing at bwrap" not in work_detail
    ok = not (missing_at_root and probed_at_work)
    print("\nverdict:", "PASS" if ok else "FAIL", "(#69: bwrap rejected as workspace path when cwd is an ancestor)")
    return 0 if ok else 1


if __name__ == "__main__":
    raise SystemExit(main())
