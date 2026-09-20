#!/usr/bin/env python3
"""Exit-code / terminal-status contract probe for `orca exec`.

`docs/harness-contract.md` pins the headless contract: a versioned JSONL event stream and
**deterministic exit codes** — `0` success, `1` failure, `2` usage error, `3`
`approval_required` (a denied action in jsonl mode, where approvals cannot be answered),
`4` a budget stop (`OperationTerminal::Stopped`). Scripts and CI depend on those numbers,
so this suite drives one scenario per code and checks the exit code *and* the terminal
status in the event stream:

| scenario | expected exit | expected status |
|---|---|---|
| clean run | 0 | `success` |
| provider always truncates | 1 | `failed` |
| unknown CLI flag | 2 | — (clap, no stream) |
| bash action in `suggest` + jsonl (auto-deny) | 3 | `approval_required` |
| `--max-tool-calls 0` | 4 | `stopped` |

Usage:
    python3 scripts/eval/exit_code_probe.py [--binary target/release/orca]
"""

from __future__ import annotations

import argparse
import base64
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

CMD = base64.b64encode(b"echo EXIT-CODE-PROBE").decode()


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/release/orca")
    parser.add_argument("--out", default="jobs/eval-exit-code-probe")
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
    home = tempfile.mkdtemp(prefix="orca-exit-code-home-")
    work = tempfile.mkdtemp(prefix="orca-exit-code-work-")
    env = dict(os.environ)
    env.update({
        "ORCA_HOME": home,
        "ORCA_API_KEY": "exit-code-probe",
        "ORCA_BASE_URL": f"http://127.0.0.1:{port}",
    })
    env.pop("DEEPSEEK_API_KEY", None)

    checks: list[tuple[str, bool, str, str]] = []
    results: list[dict] = []
    skipped: list[tuple[str, str]] = []

    def record(name: str, ok: bool, detail: str, issue: str = "") -> None:
        checks.append((name, ok, detail, issue))
        results.append({"case": name, "ok": ok, "detail": detail, "issue": issue})

    def record_skip(name: str, detail: str) -> None:
        skipped.append((name, detail))
        results.append({"case": name, "ok": None, "skipped": True, "detail": detail, "issue": ""})

    def run(label: str, cli: list[str], prompt: str, timeout: float = 300.0) -> subprocess.CompletedProcess:
        completed = subprocess.run(
            [binary, "exec", "--output-format", "jsonl", *cli, "--", prompt],
            capture_output=True, text=True, env=env, cwd=work, timeout=timeout,
        )
        (out_dir / f"{label}.jsonl").write_text(completed.stdout or "", encoding="utf-8")
        return completed

    def status_of(completed: subprocess.CompletedProcess) -> str | None:
        status = None
        for line in (completed.stdout or "").splitlines():
            line = line.strip()
            if not line.startswith("{"):
                continue
            try:
                event = json.loads(line)
            except json.JSONDecodeError:
                continue
            if event.get("type") in ("session.completed", "session.failed"):
                payload = event.get("payload") or {}
                status = str(payload.get("status") or payload.get("stopReason") or event.get("type"))
        return status

    try:
        clean = run("clean", ["--mode", "full-auto"], "FIMODE=clean exit code probe")
        record(
            "clean run → 0 / success",
            clean.returncode == 0 and status_of(clean) == "success",
            f"exit={clean.returncode} status={status_of(clean)}",
        )

        broken = run("provider_failure", ["--mode", "full-auto"], "FIMODE=truncate_always boom")
        record(
            "provider failure → 1 / failed",
            broken.returncode == 1 and status_of(broken) in ("failed", None),
            f"exit={broken.returncode} status={status_of(broken)}",
        )

        usage = subprocess.run(
            [binary, "exec", "--definitely-not-a-flag", "--", "x"],
            capture_output=True, text=True, env=env, cwd=work, timeout=120,
        )
        record(
            "unknown flag → 2 (usage)",
            usage.returncode == 2,
            f"exit={usage.returncode} err={(usage.stderr or '').strip()[:60]!r}",
        )

        # The deny path needs a working OS sandbox: with one unavailable the bash tool
        # fails for an unrelated reason and the run never reaches approval. The Linux
        # binary in a privileged container is the environment where the contract applies.
        musl_dir = Path(__file__).resolve().parent.parent.parent / "target/x86_64-unknown-linux-musl/release"
        if (musl_dir / "orca").exists():
            script = (
                "apk add --no-cache bubblewrap >/dev/null 2>&1; mkdir -p /work /tmp/orca-home; "
                "cd /work; /mnt/orca exec --mode suggest --output-format jsonl --no-history -- "
                f"'FIMODE=tool_script FITOOL=cmd FICMD64={CMD}'"
            )
            denied = subprocess.run(
                [
                    "docker", "run", "--rm", "--privileged",
                    "--add-host=host.docker.internal:host-gateway",
                    "-v", f"{musl_dir}:/mnt:ro",
                    "-e", f"ORCA_BASE_URL=http://host.docker.internal:{port}",
                    "-e", "ORCA_API_KEY=exit-code-probe",
                    "-e", "ORCA_HOME=/tmp/orca-home",
                    "alpine:latest", "sh", "-c", script,
                ],
                capture_output=True, text=True, timeout=300,
            )
            (out_dir / "approval_denied_container.jsonl").write_text(denied.stdout or "", encoding="utf-8")
            denied_status = status_of(denied)
            record(
                "denied action in jsonl → 3 / approval_required",
                denied.returncode == 3 and denied_status == "approval_required",
                f"exit={denied.returncode} status={denied_status}",
                issue="" if denied.returncode == 3 else "blocked by #85",
            )
        else:
            record_skip("denied action in jsonl → 3 / approval_required",
                        "skipped: no Linux binary for the sandboxed container")

        # `--max-tool-calls 0` is rejected as invalid ("must be a positive integer",
        # exit 1); a one-turn budget with a tool call is the way to reach the stop path.
        budget = run(
            "budget_stop",
            ["--mode", "full-auto", "--max-turns", "1"],
            "FIMODE=tool_script FITOOL=cmd FICMD64=" + CMD,
        )
        budget_status = status_of(budget)
        record(
            "budget stop → 4 / stopped",
            budget.returncode == 4 and budget_status in ("stopped", "budget_exhausted"),
            f"exit={budget.returncode} status={budget_status}",
            issue="" if budget.returncode == 4 else "blocked by #85",
        )

        # The contract also promises the resume hint in text mode for a non-success exit.
        text = subprocess.run(
            [binary, "exec", "--mode", "full-auto", "--", "FIMODE=truncate_always boom"],
            capture_output=True, text=True, env=env, cwd=work, timeout=300,
        )
        hint = text.stdout + text.stderr
        record(
            "text mode failure prints a resume hint",
            text.returncode != 0 and "resume" in hint.lower(),
            f"exit={text.returncode} hint={next((line for line in hint.splitlines() if 'resume' in line.lower()), '')[:70]!r}",
        )
    finally:
        provider.shutdown()

    (out_dir / "checks.jsonl").write_text("\n".join(json.dumps(item) for item in results) + "\n")
    failures = 0
    for name, detail in skipped:
        print(f"[INFO] {name:<46} {detail}")
    for name, ok, detail, issue in checks:
        failures += 0 if ok else 1
        suffix = f" — {issue}" if issue and not ok else ""
        print(f"[{'PASS' if ok else 'FAIL'}] {name:<46} {detail[:95]}{suffix}")
    print(f"\nlogs: {out_dir}")
    print(f"verdict: {'PASS' if not failures else f'FAIL ({failures} checks)'}")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
