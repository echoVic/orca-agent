#!/usr/bin/env python3
"""Session-store recovery probe: what `--continue` / `--resume` do to damaged history.

The plan covers `--continue` on a healthy history (messages 2 → 4). This suite covers the
storage failure modes a real user hits after a crash, a full disk, or a hand-edited home:

* the last write is torn (a partial JSONL record, exactly what a SIGKILL mid-append leaves);
* a record in the middle is corrupt;
* a session file exists but has no meta record;
* `--resume`/`--fork` are pointed at an id that does not exist;
* `--continue` runs against an empty `ORCA_HOME`.

In every case the contract is: fail (or recover) *cleanly* — name the session, do not
silently drop the conversation, do not hang, do not leave the process in a bad state.

Usage:
    python3 scripts/eval/session_recovery_probe.py [--binary target/release/orca]
"""

from __future__ import annotations

import argparse
import json
import os
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import mock_provider  # noqa: E402

PROMPT = "FIMODE=clean remember this"


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/release/orca")
    parser.add_argument("--out", default="jobs/eval-session-recovery-probe")
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    if not Path(binary).exists():
        print(f"binary not found: {binary}", file=sys.stderr)
        return 2

    stamp = time.strftime("%Y%m%d-%H%M%S")
    out_dir = Path(args.out) / stamp
    out_dir.mkdir(parents=True, exist_ok=True)
    port = free_port()
    provider = mock_provider.serve(port, str(out_dir / "provider-requests.jsonl"), "127.0.0.1")

    checks: list[tuple[str, bool, str, str]] = []
    results: list[dict] = []

    def record(name: str, ok: bool, detail: str, issue: str = "") -> None:
        checks.append((name, ok, detail, issue))
        results.append({"case": name, "ok": ok, "detail": detail, "issue": issue})

    def fresh_env() -> dict:
        home = tempfile.mkdtemp(prefix="orca-session-recovery-")
        env = dict(os.environ)
        env.update({
            "ORCA_HOME": home,
            "ORCA_API_KEY": "session-recovery-probe",
            "ORCA_BASE_URL": f"http://127.0.0.1:{port}",
        })
        env.pop("DEEPSEEK_API_KEY", None)
        return env

    def run_orca(env: dict, *args: str, cwd: str) -> subprocess.CompletedProcess:
        # History must stay enabled for --continue/--resume/--fork; the CLI rejects the
        # combination with --no-history ("cannot be combined"), which is correct behaviour
        # but would make every check below test the wrong thing.
        return subprocess.run(
            [binary, "exec", "--mode", "full-auto", *args, "--", "FIMODE=clean follow-up"],
            capture_output=True, text=True, env=env, cwd=cwd, timeout=300,
        )

    def sessions(home: str) -> list[Path]:
        return sorted(Path(home).rglob("session-*.jsonl"))

    def seed(env: dict, cwd: str, label: str) -> tuple[subprocess.CompletedProcess, list[Path]]:
        """One ordinary history-writing run; returns it plus the session files it created."""
        # `--save-history` is needed because jsonl output defaults to no history.
        result = subprocess.run(
            [binary, "exec", "--mode", "full-auto", "--output-format", "jsonl", "--save-history",
             "--cwd", cwd, "--", PROMPT],
            capture_output=True, text=True, env=env, cwd=cwd, timeout=300,
        )
        (out_dir / f"{label}.jsonl").write_text(result.stdout, encoding="utf-8")
        return result, sessions(env["ORCA_HOME"])

    try:
        # 0. Control: a healthy history continues.
        env = fresh_env()
        cwd = tempfile.mkdtemp(prefix="orca-session-cwd-")
        seeded, files = seed(env, cwd, "seed-healthy")
        record("history written", bool(files), f"sessions={len(files)} exit={seeded.returncode}")
        if files:
            before = len(files[-1].read_text(encoding="utf-8").splitlines())
            healthy = run_orca(env, "--continue", cwd=cwd)
            after = len(files[-1].read_text(encoding="utf-8").splitlines())
            record(
                "healthy --continue appends to the same session",
                healthy.returncode == 0 and after > before,
                f"exit={healthy.returncode} records {before}->{after} err={healthy.stderr.strip()[:50]!r}",
            )

        # 1. Torn last record (SIGKILL mid-append).
        env = fresh_env()
        cwd = tempfile.mkdtemp(prefix="orca-session-cwd-")
        _, files = seed(env, cwd, "seed-torn")
        if files:
            target = files[-1]
            payload = target.read_text(encoding="utf-8")
            target.write_text(payload[: max(1, len(payload) - 25)], encoding="utf-8")
            torn = run_orca(env, "--continue", cwd=cwd)
            text = (torn.stdout + torn.stderr).lower()
            named = any(word in text for word in ("quarantin", "corrupt", "malformed", "invalid", "unexpected"))
            # Skipping an incomplete trailing record is the conventional crash recovery;
            # what must not happen is a hang, a fresh session that pretends to be a
            # continuation, or a silent exit without either recovery or a named reason.
            record(
                "torn trailing record: recovered or named",
                torn.returncode == 0 or named,
                f"exit={torn.returncode} named_reason={named} "
                f"msg={(torn.stderr or torn.stdout).strip()[:60]!r}",
            )

        # 2. Corrupt record in the middle.
        env = fresh_env()
        cwd = tempfile.mkdtemp(prefix="orca-session-cwd-")
        _, files = seed(env, cwd, "seed-corrupt")
        if files:
            target = files[-1]
            lines = target.read_text(encoding="utf-8").splitlines()
            if len(lines) > 2:
                lines.insert(len(lines) // 2, "{not json at all")
                target.write_text("\n".join(lines) + "\n", encoding="utf-8")
            corrupt = run_orca(env, "--continue", cwd=cwd)
            text = (corrupt.stdout + corrupt.stderr).lower()
            ok = any(word in text for word in ("quarantin", "corrupt", "malformed", "invalid", "unexpected"))
            ok = ok and corrupt.returncode != 0
            record(
                "corrupt middle record reported",
                ok,
                f"exit={corrupt.returncode} msg={(corrupt.stderr or corrupt.stdout).strip()[:80]!r}",
                issue="" if ok else "blocked by #83",
            )

        # 3. Unknown resume / fork targets.
        env = fresh_env()
        cwd = tempfile.mkdtemp(prefix="orca-session-cwd-")
        unknown = run_orca(env, "--resume", "no-such-session-id", cwd=cwd)
        text = (unknown.stdout + unknown.stderr).lower()
        ok = unknown.returncode != 0 and "no saved session" in text
        record(
            "unknown --resume refused by name",
            ok,
            f"exit={unknown.returncode} msg={(unknown.stderr or unknown.stdout).strip()[:80]!r}",
        )
        fork = run_orca(env, "--fork", "no-such-session-id", cwd=cwd)
        text = (fork.stdout + fork.stderr).lower()
        ok = fork.returncode != 0 and ("no saved session" in text or "not found" in text)
        record(
            "unknown --fork refused by name",
            ok,
            f"exit={fork.returncode} msg={(fork.stderr or fork.stdout).strip()[:80]!r}",
        )

        # 4. Empty home.
        env = fresh_env()
        cwd = tempfile.mkdtemp(prefix="orca-session-cwd-")
        empty = run_orca(env, "--continue", cwd=cwd)
        text = (empty.stdout + empty.stderr).lower()
        ok = empty.returncode != 0 and ("no saved session" in text or "no sessions" in text)
        record(
            "empty home --continue refused by name",
            ok,
            f"exit={empty.returncode} msg={(empty.stderr or empty.stdout).strip()[:80]!r}",
        )
    finally:
        provider.shutdown()

    (out_dir / "checks.jsonl").write_text("\n".join(json.dumps(item) for item in results) + "\n")
    failures = 0
    for name, ok, detail, issue in checks:
        failures += 0 if ok else 1
        suffix = f" — {issue}" if issue and not ok else ""
        print(f"[{'PASS' if ok else 'FAIL'}] {name:<46} {detail[:95]}{suffix}")
    print(f"\nlogs: {out_dir}")
    print(f"verdict: {'PASS' if not failures else f'FAIL ({failures} checks)'}")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
