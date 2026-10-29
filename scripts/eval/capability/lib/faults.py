"""Fault installation for axis D (fault resilience).

Faults are installed by a `prepare` script before the agent starts, using only things that are safe on
the host: command wrappers on PATH that fail a bounded number of times, a read-only file, and a stale
lock file. Nothing here needs root, deletes user data, or fills a disk. The verifier runs with the
unpolluted PATH so a fault cannot be mistaken for a wrong answer.
"""

from __future__ import annotations

import sys
from pathlib import Path

WRAPPERS = {
    "flaky_cargo_once": ("cargo", 1, "error: failed to fetch `registry+https://github.com/rust-lang/crates.io-index`"),
    "flaky_cargo_twice": ("cargo", 2, "error: failed to fetch `registry+https://github.com/rust-lang/crates.io-index`"),
    "flaky_git_once": ("git", 1, "fatal: unable to access 'https://github.com/': Could not resolve host"),
    "flaky_mkdir_once": ("mkdir", 1, "mkdir: cannot create directory: Input/output error"),
}


def install(workspace: Path, faults: list[str]) -> list[str]:
    faultbin = workspace / ".faultbin"
    faultbin.mkdir(parents=True, exist_ok=True)
    installed = []
    for name in faults:
        if name not in WRAPPERS:
            continue
        command, failures, message = WRAPPERS[name]
        real = _which(command)
        if real is None:
            continue
        counter = faultbin / f".{command}.count"
        counter.write_text("0", encoding="utf-8")
        wrapper = faultbin / command
        wrapper.write_text(
            "#!/usr/bin/env bash\n"
            f'count_file="{counter}"\n'
            'count=$(cat "$count_file" 2>/dev/null || echo 0)\n'
            '[ "$count" -lt %d ] && { echo $((count + 1)) > "$count_file"; '
            f'echo "{message}" >&2; exit 101; }}\n' % failures
            + f'exec "{real}" "$@"\n',
            encoding="utf-8",
        )
        wrapper.chmod(0o755)
        installed.append(name)
    if "readonly_src" in faults:
        source = workspace / "src" / "lib.rs"
        if source.exists():
            source.chmod(0o444)
            installed.append("readonly_src")
    if "readonly_manifest" in faults:
        manifest = workspace / "Cargo.toml"
        if manifest.exists():
            manifest.chmod(0o444)
            installed.append("readonly_manifest")
    if "stale_lock" in faults:
        (workspace / ".cargo-lock").write_text("held by another process\n", encoding="utf-8")
        installed.append("stale_lock")
    return installed


def _which(command: str) -> str | None:
    import shutil

    return shutil.which(command)


def main() -> int:
    """`prepare` entry: read the fault list from the scenario-provided env var."""
    import json
    import os

    faults = json.loads(os.environ.get("CAPABILITY_FAULTS", "[]"))
    installed = install(Path.cwd(), faults)
    print(f"installed faults: {installed}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
