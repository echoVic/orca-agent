#!/usr/bin/env python3
"""Shell edge-case probe (L1): output encoding, signals, exit codes and streaming.

Drives `orca exec` against scripts/eval/mock_provider.py and checks that the bash
tool reports what really happened for awkward commands: binary output, signals
(TERM/SEGV), exit 255, missing trailing newline, invalid UTF-8, and empty output.
It also pins when a call returns: an ordinary pipe command that ends inside the
default pipe yield completes in the first call, and one very long line from a
command that is still alive comes back `running` and is then read to its end
through the same task.

Usage:
    python3 scripts/eval/shell_edge_probe.py [--binary target/release/orca]
"""

from __future__ import annotations

import argparse
import base64
import json
import os
import shlex
import socket
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import mock_provider  # noqa: E402

CASES = [
    ("binary_output", "head -c 200 /dev/urandom", {"state": "completed"}),
    # Pipe commands wait up to 10 s by default (pty: 1 s), so an ordinary pipe
    # command that runs past one second still finishes in the first call.
    ("default_yield_inline", "sleep 1.5; printf done", {"state": "completed", "exit_code": 0}),
    ("signal_term", "kill -TERM $$", {"state": "failed", "exit_code": 143}),
    ("signal_segv", "kill -SEGV $$", {"state": "failed", "exit_code": 139}),
    ("exit_255", "exit 255", {"state": "failed", "exit_code": 255}),
    ("no_trailing_newline", "printf 'no-newline'", {"state": "completed"}),
    ("invalid_utf8_text", "printf '\\xff\\xfe bad bytes\\n'", {"state": "completed"}),
    ("empty_output", "true", {"state": "completed"}),
]

# One huge line fills the call's output budget at once, and the call returns as
# soon as the budget is spent, so whether the command has exited by then is a
# race between the reader and the process (issue #120). Pin the contract
# instead: a zero yield returns while the command is still alive, then a wait on
# the same task reads the bounded terminal result. The launch log shows the
# command ran once.
HUGE_LINE_BYTES = 300000
HUGE_LINE_COMMAND = (
    "echo launch >> launches.log; "
    f"head -c {HUGE_LINE_BYTES} /dev/zero | tr '\\0' 'a'; sleep 2"
)


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def run_orca(binary: str, port: int, prompt: str, cwd: str, timeout: float) -> list[tuple[str, dict]]:
    """Run one `orca exec` turn and return each completed tool call's result."""
    env = dict(os.environ)
    env["PATH"] = f"{Path(binary).parent}{os.pathsep}{env.get('PATH','')}"
    env.pop("DEEPSEEK_API_KEY", None)
    with tempfile.TemporaryDirectory() as home:
        env["ORCA_HOME"] = home
        line = (
            f"orca exec --mode full-auto --output-format jsonl"
            f" --base-url http://127.0.0.1:{port} --api-key edge"
            f" --cwd {shlex.quote(cwd)} --no-history -- {shlex.quote(prompt)}"
        )
        # `orca exec` appends piped stdin to the prompt; never hand it ours.
        result = subprocess.run(
            ["/bin/sh", "-c", line],
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=timeout,
            env=env,
        )
    calls: list[tuple[str, dict]] = []
    for line in result.stdout.splitlines():
        if '"tool.call.completed"' not in line:
            continue
        payload = json.loads(line)["payload"]
        # Failed calls carry the structured result in `error`, not `output`.
        raw = payload.get("output") or payload.get("error") or ""
        try:
            detail = json.loads(raw)
        except json.JSONDecodeError:
            detail = {}
        calls.append((payload.get("name") or "", detail if isinstance(detail, dict) else {}))
    return calls


def check_huge_line(calls: list[tuple[str, dict]], cwd: str) -> tuple[list[str], str]:
    bash = [detail for name, detail in calls if name == "bash"]
    first = bash[0] if bash else {}
    task_id = first.get("task_id")
    final: dict = {}
    for name, detail in calls:
        if name != "task_wait":
            continue
        for task in detail.get("tasks") or []:
            if isinstance(task, dict) and task.get("task_id") == task_id:
                final = task
    output = final.get("output") or ""
    launches_path = Path(cwd) / "launches.log"
    launches = len(launches_path.read_text().splitlines()) if launches_path.exists() else 0

    problems = []
    if len(bash) != 1:
        problems.append(f"expected one bash call, saw {len(bash)}")
    if first.get("state") != "running" or not task_id:
        problems.append(f"zero yield returned state={first.get('state')} task={task_id}")
    if final.get("status") != "completed" or final.get("exit_code") != 0:
        problems.append(f"wait on the task saw status={final.get('status')} exit={final.get('exit_code')}")
    if final.get("output_bytes_total") != HUGE_LINE_BYTES:
        problems.append(f"task recorded {final.get('output_bytes_total')} of {HUGE_LINE_BYTES} bytes")
    if not output or len(output) >= HUGE_LINE_BYTES or final.get("truncated") is not True:
        problems.append(f"returned output is not bounded ({len(output)} bytes)")
    if launches != 1:
        problems.append(f"command launched {launches} times")
    summary = (
        f"first={first.get('state')!s:<8} final={final.get('status')!s:<10} "
        f"exit={final.get('exit_code')!s:<6} bytes={len(output)}/{final.get('output_bytes_total')} "
        f"launches={launches}"
    )
    return problems, summary


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/release/orca")
    parser.add_argument("--out", default="jobs/eval-shell-edge")
    parser.add_argument("--timeout", type=float, default=120.0)
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    if not Path(binary).exists():
        print(f"binary not found: {binary}", file=sys.stderr)
        return 2

    port = free_port()
    server = mock_provider.serve(port, f"{args.out}-requests.jsonl")
    failures = 0
    try:
        for name, command, expected in CASES:
            token = base64.b64encode(command.encode()).decode()
            prompt = f"FIMODE=tool_script FITOOL=cmd FICMD64={token}"
            with tempfile.TemporaryDirectory() as cwd:
                calls = run_orca(binary, port, prompt, cwd, args.timeout)
            detail = calls[-1][1] if calls else {}
            ok = all(detail.get(key) == value for key, value in expected.items())
            failures += 0 if ok else 1
            note = "" if ok else f"  expected {expected}"
            print(
                f"[{'PASS' if ok else 'FAIL'}] {name:<20} state={detail.get('state')!s:<10} "
                f"exit={detail.get('exit_code')!s:<6} bytes={len(detail.get('output') or '')}{note}",
                flush=True,
            )

        # The default tool_script flow runs the bash call, then waits on its task
        # while the call reports `running`.
        token = base64.b64encode(HUGE_LINE_COMMAND.encode()).decode()
        prompt = f"FIMODE=tool_script FICMD64={token} FIYIELD=0"
        with tempfile.TemporaryDirectory() as cwd:
            calls = run_orca(binary, port, prompt, cwd, args.timeout)
            problems, summary = check_huge_line(calls, cwd)
        failures += 1 if problems else 0
        note = f"  {'; '.join(problems)}" if problems else ""
        print(
            f"[{'FAIL' if problems else 'PASS'}] {'huge_line_streams':<20} {summary}{note}",
            flush=True,
        )
    finally:
        server.shutdown()
    print(f"\nverdict: {'PASS' if not failures else 'FAIL'}")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
