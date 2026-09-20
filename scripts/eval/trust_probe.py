#!/usr/bin/env python3
"""Folder-trust probe: lifecycle, inheritance, overrides and key stability.

Drives `orca trust show|add|remove` against a throwaway `ORCA_HOME` and checks the
decision model:

* show → add → show → remove → show lifecycle, idempotent remove
* a trusted parent implies a trusted child
* an explicitly untrusted child overrides a trusted parent
* a symlinked path resolves to the same decision
* a decision for a not-yet-created directory must survive the directory appearing
  (issue #75)

Usage:
    python3 scripts/eval/trust_probe.py [--binary target/release/orca]
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
    parser.add_argument("--out", default="jobs/eval-trust-probe")
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    if not Path(binary).exists():
        print(f"binary not found: {binary}", file=sys.stderr)
        return 2

    home = tempfile.mkdtemp(prefix="orca-trust-probe-")
    env = dict(os.environ)
    env["ORCA_HOME"] = home
    env.pop("DEEPSEEK_API_KEY", None)

    failures = 0

    def trust(*cli: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            [binary, "trust", *cli], capture_output=True, text=True, env=env, timeout=60
        )

    def level(path: str) -> str:
        result = trust("show", "--cwd", path)
        out = (result.stdout + result.stderr).lower()
        # "unknown (treated as untrusted)" contains both words: check unknown first.
        if "unknown" in out:
            return "unknown"
        if "untrusted" in out:
            return "untrusted"
        if "trusted" in out:
            return "trusted"
        return "?"

    def check(name: str, ok: bool, detail: str = "") -> None:
        nonlocal failures
        failures += 0 if ok else 1
        note = ""
        if not ok and "existence" in name:
            note = " (known issue #75)"
        print(f"[{'PASS' if ok else 'FAIL'}] {name:<42} {detail}{note}", flush=True)

    root = Path(tempfile.mkdtemp(prefix="orca-trust-ws-"))
    child = root / "child"
    other = Path(tempfile.mkdtemp(prefix="orca-trust-other-"))
    child.mkdir()

    check("unknown before any decision", level(str(root)) == "unknown", level(str(root)))
    trust("add", "--cwd", str(root))
    check("trusted after add", level(str(root)) == "trusted", level(str(root)))
    check("child inherits parent trust", level(str(child)) == "trusted", level(str(child)))
    trust("remove", "--cwd", str(child))
    check(
        "explicit child revocation wins",
        level(str(child)) == "untrusted" and level(str(root)) == "trusted",
        f"child={level(str(child))} parent={level(str(root))}",
    )
    check("unrelated folder still unknown", level(str(other)) == "unknown", level(str(other)))

    link = root.parent / f"orca-trust-link-{os.getpid()}"
    if not link.exists():
        link.symlink_to(root)
    check("symlink resolves to same decision", level(str(link)) == "trusted", level(str(link)))

    trust("remove", "--cwd", str(root))
    trust("remove", "--cwd", str(root))
    check("remove is idempotent", level(str(root)) == "untrusted", level(str(root)))

    # Issue #75: a decision for a path that does not exist yet must survive creation.
    pending = Path(tempfile.mkdtemp(prefix="orca-trust-pending-")) / "not-yet"
    trust("add", "--cwd", str(pending))
    before = level(str(pending))
    pending.mkdir()
    after = level(str(pending))
    check(
        "decision survives path existence change",
        before == "trusted" and after == "trusted",
        f"before={before} after={after}",
    )

    Path(args.out).parent.mkdir(parents=True, exist_ok=True)
    print(f"\nverdict: {'PASS' if not failures else 'FAIL'}")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
