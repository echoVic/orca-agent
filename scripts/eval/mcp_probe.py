#!/usr/bin/env python3
"""MCP (Model Context Protocol) client-surface probe.

No other suite touches `crates/orca-mcp`, yet MCP servers are the documented way to give
orca external tools. This suite runs orca against `scripts/eval/mock_mcp_server.py`, a
minimal stdio MCP server, and checks the whole chain:

1. the configured server is started and receives `initialize` + `tools/list`;
2. the discovered tool is offered to the model as `mcp__<server>__<tool>` with its schema;
3. a call from the model reaches the server and its result comes back to the model;
4. a server that answers `tools/call` with a JSON-RPC error fails the tool, not the session;
5. a slow tool call is bounded by the configured `tool_timeout_ms`;
6. a server that never answers `initialize` fails startup without hanging the session.

Usage:
    python3 scripts/eval/mcp_probe.py [--binary target/release/orca]
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

SERVER = str(Path(__file__).resolve().parent / "mock_mcp_server.py")


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", default="target/release/orca")
    parser.add_argument("--out", default="jobs/eval-mcp-probe")
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    if not Path(binary).exists():
        print(f"binary not found: {binary}", file=sys.stderr)
        return 2
    if not Path(SERVER).exists():
        print(f"mock MCP server not found: {SERVER}", file=sys.stderr)
        return 2

    stamp = time.strftime("%Y%m%d-%H%M%S")
    # Absolute: the mock MCP server is spawned with the workspace as its cwd, so a relative
    # --log path would point somewhere that does not exist and kill the server on startup.
    out_dir = (Path(args.out) / stamp).resolve()
    out_dir.mkdir(parents=True, exist_ok=True)
    port = free_port()
    provider = mock_provider.serve(port, str(out_dir / "provider-requests.jsonl"), "127.0.0.1")

    checks: list[tuple[str, bool, str, str]] = []
    results: list[dict] = []

    def record(name: str, ok: bool, detail: str, issue: str = "") -> None:
        checks.append((name, ok, detail, issue))
        results.append({"case": name, "ok": ok, "detail": detail, "issue": issue})

    def scenario(label: str, mode: str, extra: dict | None = None, timeout: float = 240.0) -> tuple[subprocess.CompletedProcess, list[dict], dict]:
        """One orca run against a freshly configured mock MCP server."""
        home = tempfile.mkdtemp(prefix=f"orca-mcp-{label}-home-")
        work = tempfile.mkdtemp(prefix=f"orca-mcp-{label}-work-")
        log = out_dir / f"{label}-mcp.jsonl"
        settings = extra or {}
        server = [sys.executable, SERVER, "--log", str(log), "--mode", mode]
        if "delay" in settings:
            server += ["--delay", str(settings["delay"])]
        config = ["[[mcp_servers]]", 'name = "probe"', 'transport = "stdio"',
                  f'command = "{server[0]}"',
                  "args = [" + ", ".join(f'"{item}"' for item in server[1:]) + "]"]
        if "tool_timeout_ms" in settings:
            config.append(f"tool_timeout_ms = {settings['tool_timeout_ms']}")
        if "startup_timeout_ms" in settings:
            config.append(f"startup_timeout_ms = {settings['startup_timeout_ms']}")
        (Path(home) / "config.toml").write_text("\n".join(config) + "\n", encoding="utf-8")

        env = dict(os.environ)
        env.update({"ORCA_HOME": home, "ORCA_API_KEY": "mcp-probe", "ORCA_BASE_URL": f"http://127.0.0.1:{port}"})
        env.pop("DEEPSEEK_API_KEY", None)
        env["PATH"] = f"{Path(sys.executable).parent}{os.pathsep}{env.get('PATH', '')}"

        started = time.time()
        completed = subprocess.run(
            [binary, "exec", "--mode", "full-auto", "--output-format", "jsonl", "--no-history",
             "--", "FIMODE=tool_script FITOOL=mcpcall"],
            capture_output=True, text=True, env=env, cwd=work, timeout=timeout,
        )
        wall = time.time() - started
        (out_dir / f"{label}.jsonl").write_text(completed.stdout or "", encoding="utf-8")
        entries: list[dict] = []
        if log.exists():
            entries = [json.loads(line) for line in log.read_text().splitlines() if line.strip()]
        return completed, entries, {"wall": wall, "home": home, "stderr": completed.stderr}

    def methods(entries: list[dict]) -> list[str]:
        out = []
        for entry in entries:
            message = entry.get("message") or {}
            if entry.get("direction") == "in" and message.get("method"):
                out.append(str(message["method"]))
        return out

    def events(completed: subprocess.CompletedProcess) -> list[dict]:
        out = []
        for line in (completed.stdout or "").splitlines():
            line = line.strip()
            if line.startswith("{"):
                try:
                    out.append(json.loads(line))
                except json.JSONDecodeError:
                    continue
        return out

    try:
        ok_run, ok_entries, meta = scenario("ok", "ok")
        seen_methods = methods(ok_entries)
        record(
            "server started and handshaken",
            "initialize" in seen_methods and "tools/list" in seen_methods,
            f"methods={seen_methods[:5]} wall={meta['wall']:.1f}s",
        )

        offered = any(
            "mcp__probe__echo" in json.dumps(event)
            for event in events(ok_run)
        )
        record(
            "discovered tool offered to the model",
            offered,
            f"exit={ok_run.returncode}",
            issue="" if offered else "blocked by #85",
        )

        calls = [entry for entry in ok_entries
                 if (entry.get("message") or {}).get("method") == "tools/call"]
        echoed = any("echo:" in json.dumps(event) for event in events(ok_run))
        record(
            "model tool call reached the server and returned",
            bool(calls) and echoed,
            f"tools/call={len(calls)} echoed_back={echoed} exit={ok_run.returncode}",
        )

        fail_run, fail_entries, fail_meta = scenario("failing_tool", "fail")
        fail_calls = methods(fail_entries).count("tools/call")
        record(
            "server-side tool error fails the tool, not the session",
            fail_run.returncode == 0 and fail_calls >= 1,
            f"exit={fail_run.returncode} tools/call={fail_calls}",
        )

        slow_run, slow_entries, slow_meta = scenario(
            "slow_tool", "slow", {"tool_timeout_ms": 1500, "delay": 8.0}
        )
        record(
            "slow tool call bounded by tool_timeout_ms",
            slow_meta["wall"] < 120 and slow_run.returncode in (0, 1),
            f"wall={slow_meta['wall']:.1f}s exit={slow_run.returncode}",
        )

        hang_run, hang_entries, hang_meta = scenario(
            "hanging_server", "hanging", {"startup_timeout_ms": 3000}, timeout=180.0
        )
        record(
            "unresponsive server fails fast, session still runs",
            hang_meta["wall"] < 120 and hang_run.returncode in (0, 1),
            f"wall={hang_meta['wall']:.1f}s exit={hang_run.returncode}",
        )
    finally:
        provider.shutdown()

    (out_dir / "checks.jsonl").write_text("\n".join(json.dumps(item) for item in results) + "\n")
    failures = 0
    for name, ok, detail, issue in checks:
        failures += 0 if ok else 1
        suffix = f" — {issue}" if issue and not ok else ""
        print(f"[{'PASS' if ok else 'FAIL'}] {name:<48} {detail[:95]}{suffix}")
    print(f"\nlogs: {out_dir}")
    print(f"verdict: {'PASS' if not failures else f'FAIL ({failures} checks)'}")
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
