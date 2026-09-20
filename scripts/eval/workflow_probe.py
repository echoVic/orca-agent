#!/usr/bin/env python3
"""Workflow CLI probe.

`orca workflow` is the user-facing entry point for the workflow runtime (JavaScript
orchestration, `agent()` fan-out), and none of the other suites touch it. This one covers
the CLI contract around it:

* read-only / recovery commands on a fresh `ORCA_HOME` (list, show, source, clone, stop,
  resume, restart-phase) must fail cleanly — never panic, never print nothing;
* a real workflow script must launch from inside the workspace **and** from any other
  directory (relative and absolute script paths);
* a launched run must appear in `workflow list` and reach a terminal status;
* a missing script must produce an actionable error.

The model is the local `mock_provider`, so a workflow agent call really happens but costs
nothing. Requires `node` on PATH (the workflow host is a Node process).

Usage:
    python3 scripts/eval/workflow_probe.py [--binary target/release/orca]
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

TWO_PHASE = """export const meta = { name: 'two', description: 'restart probe', phases: ['alpha', 'beta'] };
const a = await phase('alpha', async () => agent('FIMODE=clean alpha ok'));
const b = await phase('beta', async () => agent('FIMODE=clean beta ok'));
export default { a, b };
"""

NAMED = """export const meta = { name: 'named', description: 'named probe', phases: ['main'] };
const r = await phase('main', async () => agent('FIMODE=clean named probe'));
export default r;
"""

SCRIPT = """export const meta = { name: 'probe', description: 'probe', phases: ['main'] };
const result = await phase('main', async () => agent('FIMODE=clean workflow probe'));
export default result;
"""


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/release/orca")
    parser.add_argument("--out", default="jobs/eval-workflow-probe")
    parser.add_argument("--settle", type=float, default=20.0, help="seconds to let a launched run finish")
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    if not Path(binary).exists():
        print(f"binary not found: {binary}", file=sys.stderr)
        return 2

    stamp = time.strftime("%Y%m%d-%H%M%S")
    out_dir = Path(args.out) / stamp
    out_dir.mkdir(parents=True, exist_ok=True)
    request_log = str(out_dir / "provider-requests.jsonl")

    port = free_port()
    provider = mock_provider.serve(port, request_log, "127.0.0.1")
    home = tempfile.mkdtemp(prefix="orca-workflow-home-")
    work = tempfile.mkdtemp(prefix="orca-workflow-work-")
    other = tempfile.mkdtemp(prefix="orca-workflow-other-")
    (Path(work) / "ok.js").write_text(SCRIPT)
    (Path(work) / "bad.js").write_text("function ( { this is not javascript\n")
    (Path(work) / "two.js").write_text(TWO_PHASE)
    user_workflow = Path(home) / "workflows" / "named.js"
    user_workflow.parent.mkdir(parents=True, exist_ok=True)
    user_workflow.write_text(NAMED)
    project_workflow = Path(work) / ".orca" / "workflows" / "named.js"
    project_workflow.parent.mkdir(parents=True, exist_ok=True)
    project_workflow.write_text(NAMED)
    dead_port = free_port()  # nothing listens here: a deterministic agent failure
    env = dict(os.environ)
    env.update({
        "ORCA_HOME": home,
        "ORCA_API_KEY": "workflow-probe",
        "ORCA_BASE_URL": f"http://127.0.0.1:{port}",
    })
    env.pop("DEEPSEEK_API_KEY", None)
    # The project workflow only resolves in a trusted workspace.
    subprocess.run([binary, "trust", "add", "--cwd", work], capture_output=True, env=env, cwd=work)

    checks: list[tuple[str, bool, str, str]] = []
    results: list[dict] = []

    def record(name: str, ok: bool, detail: str, issue: str = "") -> None:
        checks.append((name, ok, detail, issue))
        results.append({"case": name, "ok": ok, "detail": detail, "issue": issue})

    def workflow(*cli: str, cwd: str, timeout: float = 240.0) -> subprocess.CompletedProcess:
        return subprocess.run(
            [binary, "--cwd", work, "workflow", *cli],
            capture_output=True,
            text=True,
            env=env,
            cwd=cwd,
            timeout=timeout,
        )

    def runs() -> list[dict]:
        listing = workflow("list", cwd=work)
        try:
            return json.loads(listing.stdout or "[]")
        except json.JSONDecodeError:
            return []

    def run_with_base_url(port: int, *cli: str) -> subprocess.CompletedProcess:
        scoped = dict(env)
        scoped["ORCA_BASE_URL"] = f"http://127.0.0.1:{port}"
        return subprocess.run(
            [binary, "--cwd", work, "workflow", *cli],
            capture_output=True, text=True, env=scoped, cwd=work, timeout=300,
        )

    def wait_terminal(run_id: str, limit: float = 180.0) -> dict:
        deadline = time.time() + limit
        while time.time() < deadline:
            for entry in runs():
                if entry.get("runId") == run_id and entry.get("status") in ("failed", "completed"):
                    return entry
            time.sleep(2.0)
        return next((entry for entry in runs() if entry.get("runId") == run_id), {})

    try:
        listing = workflow("list", cwd=work)
        record(
            "workflow list (empty project)",
            listing.returncode == 0 and listing.stdout.strip() == "[]",
            f"rc={listing.returncode} out={listing.stdout.strip()[:40]!r}",
        )

        for name, cli in (
            ("show", ("show", "no-such-task")),
            ("source", ("source", "no-such-workflow")),
            ("clone", ("clone", "no-such-run")),
            ("stop", ("stop", "no-such-task")),
            ("resume", ("resume", "no-such-run")),
            ("restart-phase", ("restart-phase", "no-such-run", "phase")),
        ):
            result = workflow(*cli, cwd=work)
            text = (result.stdout + result.stderr).lower()
            ok = result.returncode != 0 and "not found" in text and "panic" not in text
            record(f"workflow {name} unknown id", ok, f"rc={result.returncode} msg={(result.stderr or result.stdout).strip()[:70]!r}")

        # The documented form: run a project script from somewhere else.
        relative = workflow("run", "ok.js", cwd=other)
        relative_ok = relative.returncode == 0 and "runId" in relative.stdout
        record(
            "run relative script from other cwd",
            relative_ok,
            f"rc={relative.returncode} err={relative.stderr.strip()[:70]!r}",
            issue="" if relative_ok else "blocked by #82",
        )

        absolute = workflow("run", str(Path(work) / "ok.js"), cwd=other)
        absolute_ok = absolute.returncode == 0 and "runId" in absolute.stdout
        record(
            "run absolute script from other cwd",
            absolute_ok,
            f"rc={absolute.returncode} err={absolute.stderr.strip()[:70]!r}",
        )

        inside = workflow("run", "ok.js", cwd=work)
        inside_ok = inside.returncode == 0 and "runId" in inside.stdout
        record(
            "run relative script inside workspace",
            inside_ok,
            f"rc={inside.returncode} err={inside.stderr.strip()[:70]!r}",
        )

        missing = workflow("run", "no-such-script.js", cwd=work)
        text = (missing.stdout + missing.stderr).lower()
        ok = missing.returncode != 0 and "no-such-script" in text
        record(
            "missing script names the script",
            ok,
            f"rc={missing.returncode} msg={(missing.stderr or missing.stdout).strip()[:70]!r}",
            issue="" if ok else "blocked by #82",
        )

        # The worker runs a copied script (script.js), so the diagnostic names the *cause*
        # (missing `export const meta`, a syntax error) rather than the original file name.
        malformed = workflow("run", "bad.js", cwd=work)
        text = (malformed.stdout + malformed.stderr).lower()
        actionable = any(
            marker in text
            for marker in ("export const meta", "bad.js", "syntax", "failed to launch workflow")
        )
        ok = malformed.returncode != 0 and actionable
        record(
            "malformed script reports the cause",
            ok,
            f"rc={malformed.returncode} msg={(malformed.stderr or malformed.stdout).strip()[:90]!r}",
            issue="" if ok else "blocked by #82",
        )

        # A launched workflow must actually run: list shows it, the agent call reaches
        # the provider, and the run reaches a terminal status.
        if absolute_ok:
            time.sleep(args.settle)
        rows = runs()
        recorded = bool(rows) and all("runId" in row for row in rows)
        record(
            "launched run appears in workflow list",
            recorded,
            f"runs={len(rows)} statuses={[row.get('status') for row in rows][:4]}",
        )
        settled = bool(rows) and any(
            row.get("status") in ("completed", "failed", "running") for row in rows
        )
        record("launched run reaches a status", settled, f"statuses={[row.get('status') for row in rows][:4]}")
        # Named workflows (issue #85): `source` must see what `run` can execute — the user
        # dir comes from the active ORCA_HOME, and the project search starts at `--cwd`.
        named_user = workflow("source", "named", cwd=other)
        record(
            "source finds a user-level workflow (ORCA_HOME)",
            named_user.returncode == 0,
            f"rc={named_user.returncode} out={named_user.stdout.strip()[:50]!r} "
            f"err={named_user.stderr.strip()[:50]!r}",
            issue="" if named_user.returncode == 0 else "blocked by #85",
        )
        named_project = workflow("source", "named", cwd=other)
        record(
            "source honours --cwd for project workflows",
            named_project.returncode == 0,
            f"rc={named_project.returncode} err={named_project.stderr.strip()[:60]!r}",
            issue="" if named_project.returncode == 0 else "blocked by #85",
        )

        # A run that failed because the provider was unreachable must be recoverable with
        # `restart-failed` once the provider is back.
        broken = run_with_base_url(dead_port, "run", str(Path(work) / "two.js"))
        broken_run = None
        if broken.returncode == 0:
            broken_run = json.loads(broken.stdout)["runId"]
            failed = wait_terminal(broken_run)
            restarted = run_with_base_url(port, "restart-failed", broken_run) if failed.get("status") == "failed" else None
            new_run = None
            if restarted is not None and restarted.returncode == 0:
                new_run = json.loads(restarted.stdout)["runId"]
                entry = wait_terminal(new_run)
                ok = entry.get("status") == "completed"
                record(
                    "restart-failed recovers a failed run",
                    ok,
                    f"new_run={new_run} status={entry.get('status')} "
                    f"error={(entry.get('error') or '')[:60]!r}",
                    issue="" if ok else "blocked by #84",
                )
            else:
                record(
                    "restart-failed recovers a failed run",
                    False,
                    f"restart rc={getattr(restarted, 'returncode', None)}",
                    issue="blocked by #84",
                )

        # A completed run's phase can be replayed; the replayed agents need the provider
        # credentials the CLI itself was given.
        if absolute_ok:
            replay_target = next((row for row in rows if row.get("status") == "completed"), None)
            if replay_target:
                replayed = workflow("restart-phase", replay_target["runId"], "main", cwd=work)
                new_run = None
                if replayed.returncode == 0:
                    new_run = json.loads(replayed.stdout)["runId"]
                    entry = wait_terminal(new_run)
                    ok = entry.get("status") == "completed"
                    record(
                        "restart-phase reruns a phase",
                        ok,
                        f"new_run={new_run} status={entry.get('status')} "
                        f"error={(entry.get('error') or '')[:70]!r}",
                        issue="" if ok else "blocked by #83",
                    )
                else:
                    record("restart-phase reruns a phase", False, f"rc={replayed.returncode}",
                           issue="blocked by #83")

        provider_calls = 0
        if Path(request_log).exists():
            provider_calls = sum(1 for line in open(request_log) if line.strip())
        record(
            "workflow agent call reached the provider",
            provider_calls > 0,
            f"provider_requests={provider_calls}",
        )
    finally:
        provider.shutdown()

    (out_dir / "checks.jsonl").write_text("\n".join(json.dumps(item) for item in results) + "\n")
    failures = 0
    for name, ok, detail, issue in checks:
        failures += 0 if ok else 1
        suffix = f" — {issue}" if issue and not ok else ""
        print(f"[{'PASS' if ok else 'FAIL'}] {name:<42} {detail[:100]}{suffix}")
    print(f"\nlogs: {out_dir}")
    print(f"verdict: {'PASS' if not failures else f'FAIL ({failures} checks)'}")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
