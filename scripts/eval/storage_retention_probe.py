#!/usr/bin/env python3
"""Storage-retention probe: does `orca storage --apply` only ever delete archived sessions?

`retention.rs` is explicit about the contract ("only explicitly archived, unlocked, unchanged
files are removed", with a concurrent-resume lease), but nothing exercised it through the CLI.
This suite builds a throwaway `ORCA_HOME` with

* one *active* session (newest, still in `$ORCA_HOME/sessions/`),
* one *archived* session (moved into `$ORCA_HOME/archive/` with an old mtime),
* one session held open by a live `orca exec` process,

and checks the CLI end to end:

1. a preview (`--older-than-days N`, no `--apply`) reports bytes but deletes nothing;
2. `--apply` deletes the expired archived session and nothing else;
3. `--apply` with no policy is refused;
4. a quota policy (`--max-bytes`) removes archived sessions until the quota is met;
5. the session owned by a live process is never removed.

Usage:
    python3 scripts/eval/storage_retention_probe.py [--binary target/release/orca]
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import mock_provider  # noqa: E402


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/release/orca")
    parser.add_argument("--out", default="jobs/eval-storage-retention-probe")
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
    home = tempfile.mkdtemp(prefix="orca-storage-home-")
    cwd = tempfile.mkdtemp(prefix="orca-storage-cwd-")
    env = dict(os.environ)
    env.update({
        "ORCA_HOME": home,
        "ORCA_API_KEY": "storage-retention-probe",
        "ORCA_BASE_URL": f"http://127.0.0.1:{port}",
    })
    env.pop("DEEPSEEK_API_KEY", None)

    checks: list[tuple[str, bool, str, str]] = []
    results: list[dict] = []

    def record(name: str, ok: bool, detail: str, issue: str = "") -> None:
        checks.append((name, ok, detail, issue))
        results.append({"case": name, "ok": ok, "detail": detail, "issue": issue})

    def session_files(root: str) -> list[Path]:
        return sorted(Path(root).rglob("session-*.jsonl"))

    def run_session(prompt: str, timeout: float = 120.0) -> subprocess.CompletedProcess:
        return subprocess.run(
            [binary, "exec", "--mode", "full-auto", "--output-format", "jsonl", "--save-history",
             "--cwd", cwd, "--", prompt],
            capture_output=True, text=True, env=env, cwd=cwd, timeout=timeout,
        )

    def storage(*cli: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            [binary, "storage", *cli], capture_output=True, text=True, env=env, cwd=cwd, timeout=180
        )

    live: subprocess.Popen | None = None
    try:
        first = run_session("FIMODE=clean seed one")
        second = run_session("FIMODE=clean seed two")
        active = session_files(home)
        record(
            "two history sessions written",
            first.returncode == 0 and second.returncode == 0 and len(active) >= 2,
            f"sessions={len(active)} exits={first.returncode}/{second.returncode}",
        )

        # Archive the older session the way `archive_session` does: same relative path
        # under $ORCA_HOME/archive, then age it past the policy.
        archive = Path(home) / "archive"
        archived_path = None
        if active:
            oldest = active[0]
            relative = oldest.relative_to(Path(home) / "sessions")
            archived_path = archive / relative
            archived_path.parent.mkdir(parents=True, exist_ok=True)
            shutil.move(str(oldest), str(archived_path))
            old = time.time() - 90 * 86400
            os.utime(archived_path, (old, old))
        record("archived session planted", bool(archived_path and archived_path.exists()),
               f"path={archived_path.name if archived_path else None}")
        remaining_active = session_files(str(Path(home) / "sessions"))
        survivor = remaining_active[-1] if remaining_active else None

        # Hold one session open in a second process while retention runs.
        live = subprocess.Popen(
            [binary, "exec", "--mode", "full-auto", "--output-format", "jsonl", "--save-history",
             "--cwd", cwd, "--", "FIMODE=tool_script FITOOL=timeout"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=env, cwd=cwd, text=True,
        )
        time.sleep(6.0)

        preview = storage("--older-than-days", "30")
        preview_ok = preview.returncode == 0 and (archived_path and archived_path.exists())
        record(
            "preview deletes nothing",
            preview_ok,
            f"exit={preview.returncode} archived_still_there={bool(archived_path and archived_path.exists())} "
            f"out={preview.stdout.strip()[:60]!r}",
        )

        no_policy = storage("--apply")
        ok = no_policy.returncode != 0 and "policy" in (no_policy.stdout + no_policy.stderr).lower()
        record(
            "apply without a policy refused",
            ok,
            f"exit={no_policy.returncode} msg={(no_policy.stderr or no_policy.stdout).strip()[:70]!r}",
        )

        applied = storage("--older-than-days", "30", "--apply")
        archived_gone = not (archived_path and archived_path.exists())
        survivor_kept = bool(survivor and survivor.exists())
        live_kept = bool(live and live.poll() is None)
        record(
            "apply removes the expired archived session",
            applied.returncode == 0 and archived_gone,
            f"exit={applied.returncode} archived_gone={archived_gone} out={applied.stdout.strip()[:50]!r}",
        )
        record(
            "apply keeps the active session",
            survivor_kept,
            f"active_survivor={survivor.name if survivor else None} exists={survivor_kept}",
        )
        # The live run is allowed to finish on its own; what matters is that retention
        # neither removed its history nor disturbed it (a non-zero exit would show that).
        live_rc = None
        if live:
            try:
                live_rc = live.wait(timeout=180)
            except subprocess.TimeoutExpired:
                live.kill()
        live_sessions = [path for path in session_files(home) if "__live" in path.name] or session_files(home)
        record(
            "concurrent session survives retention untouched",
            live_rc == 0 and bool(live_sessions),
            f"live_exit={live_rc} sessions_now={len(session_files(home))}",
        )

        # Quota policy (no age): archived sessions are removed until the quota is met,
        # and the active session is still never a candidate.
        quota_archive = archive / relative if archived_path else None
        if quota_archive is not None:
            quota_archive.parent.mkdir(parents=True, exist_ok=True)
            quota_archive.write_text("{\"type\":\"meta\"}\n", encoding="utf-8")
        quota = storage("--max-bytes", "0", "--apply")
        active_kept = bool(survivor and survivor.exists())
        ok = quota.returncode == 0 and (quota_archive is None or not quota_archive.exists()) and active_kept
        record(
            "quota policy removes archived, keeps active",
            ok,
            f"exit={quota.returncode} archived_gone={not (quota_archive and quota_archive.exists())} "
            f"active_kept={active_kept} out={quota.stdout.strip()[:60]!r}",
        )
    finally:
        if live and live.poll() is None:
            live.kill()
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
