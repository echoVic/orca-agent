#!/usr/bin/env python3
"""Adversarial prompt battery for `orca exec` (L1 in docs/evaluation-plan.md).

Builds the exact command line the Terminal-Bench adapter builds
(`orca exec … -- {shlex.quote(instruction)}`), runs it through a POSIX shell, and
checks that the prompt reaches the model **byte-for-byte** with no shell
interpretation. Issue #61 (a leading `-` aborting the CLI) is the class this
guards against; the injection cases also assert that shell metacharacters in a
prompt never execute.

Usage:
    python3 scripts/eval/prompt_contract.py [--binary target/release/orca]
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shlex
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import mock_provider  # noqa: E402

CASES = [
    {"name": "leading_dash", "prompt": "- leading dash prompt", "note": "issue #61"},
    {"name": "leading_double_dash", "prompt": "--flag style opening", "note": "flag-like prompt"},
    {"name": "newlines", "prompt": "line one\nline two\nline three", "note": "embedded newlines"},
    {"name": "unicode", "prompt": "emoji 🚀 CJK 中文 accents éü", "note": "non-ASCII survives"},
    {"name": "quotes", "prompt": "it's a 'quoted' \"thing\"", "note": "mixed quoting"},
    {"name": "backslashes", "prompt": "C:\\path\\to\\file \\n literal \\t", "note": "backslash handling"},
    {"name": "tabs_and_cr", "prompt": "a\tb\rc", "note": "control characters"},
    {
        "name": "shell_injection",
        "prompt": "$(touch /tmp/orca-prompt-pwned) `touch /tmp/orca-prompt-pwned2` ; echo hi &",
        "note": "metacharacters must not execute",
        "forbid_paths": ["/tmp/orca-prompt-pwned", "/tmp/orca-prompt-pwned2"],
    },
    {"name": "long_prompt", "prompt": "L" + "x" * 100_000, "note": "100 KB instruction"},
    {
        "name": "leading_whitespace",
        "prompt": "\n  indented opening line\nbody\n",
        "note": "delivered byte-for-byte (no trim)",
    },
    {
        "name": "whitespace_only",
        "prompt": "   ",
        "note": "a blank instruction is rejected before any model turn",
        "expect_exit": 1,
        "expect_no_request": True,
    },
]


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def run_case(binary: str, port: int, case: dict, timeout: float, out_dir: Path) -> dict:
    for path in case.get("forbid_paths", []):
        Path(path).unlink(missing_ok=True)
    with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as cwd:
        prompt = case["prompt"]
        # Same construction as terminal_bench/orca_agent.py.
        command = (
            f"orca exec --mode full-auto --output-format jsonl"
            f" --base-url http://127.0.0.1:{port} --api-key prompt-battery"
            f" --cwd {shlex.quote(cwd)} --no-history"
            f" -- {shlex.quote(prompt)}"
        )
        env = dict(os.environ)
        env["ORCA_HOME"] = home
        env["PATH"] = f"{Path(binary).parent}{os.pathsep}{env.get('PATH','')}"
        env.pop("DEEPSEEK_API_KEY", None)
        started = time.time()
        try:
            completed = subprocess.run(
                ["/bin/sh", "-c", command],
                capture_output=True,
                text=True,
                timeout=timeout,
                env=env,
            )
            stdout, stderr, code = completed.stdout, completed.stderr, completed.returncode
        except subprocess.TimeoutExpired:
            stdout = stderr = ""
            code = None
    (out_dir / f"{case['name']}.jsonl").write_text(stdout, encoding="utf-8")
    status = None
    for line in stdout.splitlines():
        if '"session.completed"' in line:
            try:
                status = json.loads(line)["payload"].get("status")
            except (json.JSONDecodeError, KeyError):
                pass
    return {
        "exit_code": code,
        "status": status,
        "stderr_tail": stderr[-200:],
        "wall_s": round(time.time() - started, 2),
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/release/orca")
    parser.add_argument("--out", default="jobs/eval-prompt-contract")
    parser.add_argument("--timeout", type=float, default=120.0)
    args = parser.parse_args()

    binary = str(Path(args.binary).resolve())
    if not Path(binary).exists():
        print(f"binary not found: {binary}", file=sys.stderr)
        return 2

    stamp = time.strftime("%Y%m%d-%H%M%S")
    out_dir = Path(args.out) / stamp
    out_dir.mkdir(parents=True, exist_ok=True)
    request_log = out_dir / "requests.jsonl"
    port = free_port()
    server = mock_provider.serve(port, str(request_log))

    results = []
    try:
        for case in CASES:
            # Only requests made *by this case* count: the provider log is cumulative.
            before = request_log.stat().st_size if request_log.exists() else 0
            record = run_case(binary, port, case, args.timeout, out_dir)
            entries = []
            if request_log.exists():
                with request_log.open(encoding="utf-8") as handle:
                    handle.seek(before)
                    entries = [json.loads(line) for line in handle if line.strip()]
            expected = hashlib.sha256(case["prompt"].encode()).hexdigest()
            delivered = [e.get("prompt_sha256") for e in entries]
            notes = []
            ok = True
            expected_exit = case.get("expect_exit", 0)
            if record["exit_code"] != expected_exit:
                ok = False
                notes.append(f"exit={record['exit_code']} (expected {expected_exit})")
            if case.get("expect_no_request"):
                if entries:
                    ok = False
                    notes.append(f"{len(entries)} model request(s) despite rejection")
            elif record["status"] != "success":
                ok = False
                notes.append(f"session status={record['status']}")
            if case.get("expect_no_request"):
                pass  # nothing is delivered by design; the rejection is the assertion
            elif expected not in delivered:
                if case.get("allow_normalized"):
                    notes.append(
                        f"normalized by orca (sent {len(case['prompt'])} chars / "
                        f"{expected[:12]}, delivered {delivered[-1][:12] if delivered else '-'})"
                    )
                else:
                    ok = False
                    notes.append(
                        f"prompt not delivered verbatim (sent sha {expected[:12]}, "
                        f"got {[d[:12] for d in delivered[:2]]})"
                    )
            for path in case.get("forbid_paths", []):
                if Path(path).exists():
                    ok = False
                    notes.append(f"injection executed: {path}")
            results.append(
                {
                    "case": case["name"],
                    "note": case["note"],
                    "verdict": "PASS" if ok else "FAIL",
                    "notes": notes,
                    "result": record,
                    "requests": len(entries),
                }
            )
            print(
                f"[{'PASS' if ok else 'FAIL'}] {case['name']:<18} exit={record['exit_code']} "
                f"status={record['status']} requests={len(entries)} {'; '.join(notes)}",
                flush=True,
            )
    finally:
        server.shutdown()

    report = {
        "binary": binary,
        "started_at": stamp,
        "cases": results,
        "summary": {
            "pass": len([r for r in results if r["verdict"] == "PASS"]),
            "fail": len([r for r in results if r["verdict"] == "FAIL"]),
        },
    }
    (out_dir / "report.json").write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(f"\nreport: {out_dir / 'report.json'}")
    return 1 if report["summary"]["fail"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
