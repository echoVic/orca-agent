#!/usr/bin/env python3
"""Folder-trust concurrency probe.

`trust_probe.py` covers the single-process lifecycle (add / inherit / override /
revoke / idempotence). This suite covers the case the plan flagged as uncovered:
**several `orca trust` processes writing the same store at once**.

The store is a single TOML file rewritten by an unlocked read-modify-write
(`folder_trust.rs`: `load()` → `insert` → `atomic_write`), so the last writer wins:

* 16 concurrent `orca trust add` calls all exit 0, yet only one decision survives.
* A revoke racing concurrent adds is silently dropped in some runs — the folder the
  user just marked untrusted stays trusted, while both commands report success.

The add-race check is deterministic (one survivor out of sixteen) and is the one that
counts towards the sweep; the revoke race is reported as INFO because it is timing
dependent.

Usage:
    python3 scripts/eval/trust_race_probe.py [--binary target/release/orca] [--trials 10]
"""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
import tempfile
from pathlib import Path


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/release/orca")
    parser.add_argument("--writers", type=int, default=16)
    parser.add_argument("--trials", type=int, default=10)
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    if not Path(binary).exists():
        print(f"binary not found: {binary}", file=sys.stderr)
        return 2

    failures = 0
    infos: list[str] = []

    def run(cli: list[str], env: dict) -> subprocess.CompletedProcess:
        return subprocess.run(
            [binary, "trust", *cli], capture_output=True, text=True, env=env, timeout=60
        )

    def level(path: Path, env: dict) -> str:
        out = run(["show", "--cwd", str(path)], env).stdout.lower()
        if "unknown" in out:
            return "unknown"
        if "untrusted" in out:
            return "untrusted"
        return "trusted" if "trusted" in out else "?"

    def new_env() -> dict:
        home = tempfile.mkdtemp(prefix="orca-trust-race-home-")
        env = dict(os.environ)
        env["ORCA_HOME"] = home
        return env

    # 1. Concurrent adds: every command must survive, not just the last one.
    env = new_env()
    dirs = [Path(tempfile.mkdtemp(prefix=f"orca-trust-race-{index:02d}-")) for index in range(args.writers)]
    processes = [
        subprocess.Popen(
            [binary, "trust", "add", "--cwd", str(path)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            env=env,
        )
        for path in dirs
    ]
    codes = [process.wait(timeout=60) for process in processes]
    kept = [path for path in dirs if level(path, env) == "trusted"]
    ok = len(kept) == len(dirs) and set(codes) == {0}
    failures += 0 if ok else 1
    print(
        f"[{'PASS' if ok else 'FAIL'}] concurrent trust add          "
        f"{len(kept)}/{len(dirs)} decisions kept, exit codes={sorted(set(codes))}"
        + ("" if ok else " — blocked by #81")
    )

    # 2. A revoke racing concurrent adds: revoking trust must not be silently dropped.
    lost_revokes = 0
    for _ in range(args.trials):
        env = new_env()
        victim = Path(tempfile.mkdtemp(prefix="orca-trust-victim-"))
        others = [Path(tempfile.mkdtemp(prefix=f"orca-trust-other-{index}-")) for index in range(4)]
        run(["add", "--cwd", str(victim)], env)
        processes = [
            subprocess.Popen(
                [binary, "trust", "add", "--cwd", str(path)],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                env=env,
            )
            for path in others
        ]
        processes.append(
            subprocess.Popen(
                [binary, "trust", "remove", "--cwd", str(victim)],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                env=env,
            )
        )
        for process in processes:
            process.wait(timeout=60)
        if level(victim, env) == "trusted":
            lost_revokes += 1
    infos.append(
        f"revoke racing {4} concurrent adds: dropped in {lost_revokes}/{args.trials} trials "
        f"(timing dependent, same root cause as #81)"
    )
    print(f"[INFO] revoke race                {infos[-1]}")

    print(f"\nverdict: {'PASS' if not failures else f'FAIL ({failures} checks)'}")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
