#!/usr/bin/env python3
"""Minimal MCP (Model Context Protocol) stdio server for the evaluation probes.

Speaks newline-delimited JSON-RPC 2.0 on stdin/stdout and implements just enough for
orca's client: `initialize`, `tools/list`, `tools/call` (plus `notifications/initialized`
and `ping`). Behaviour is controlled by argv:

    mock_mcp_server.py --log PATH --mode {ok,fail,hanging,slow} --tool echo

* `ok`      — `tools/call` returns `{"echo": <arguments>}`
* `fail`    — `tools/call` returns a JSON-RPC error
* `slow`    — sleeps `--delay` seconds before answering `tools/call`
* `hanging` — never answers `initialize` (for startup-timeout checks)

Every inbound message is appended to the log as one JSON object per line, so a probe can
assert what the client actually asked for.
"""

from __future__ import annotations

import argparse
import json
import sys
import time
from pathlib import Path

PROTOCOL_VERSION = "2024-11-05"


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--log", default=None)
    parser.add_argument("--mode", default="ok", choices=["ok", "fail", "slow", "hanging"])
    parser.add_argument("--tool", default="echo")
    parser.add_argument("--delay", type=float, default=2.0)
    args = parser.parse_args()

    log_path = Path(args.log) if args.log else None

    def log(entry: dict) -> None:
        if log_path is None:
            return
        log_path.parent.mkdir(parents=True, exist_ok=True)
        with log_path.open("a", encoding="utf-8") as handle:
            handle.write(json.dumps(entry) + "\n")

    def send(message: dict) -> None:
        sys.stdout.write(json.dumps(message) + "\n")
        sys.stdout.flush()

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            log({"direction": "in", "unparsable": line[:200]})
            continue
        log({"direction": "in", "message": message})
        method = message.get("method")
        request_id = message.get("id")
        if request_id is None:  # notification
            continue
        if args.mode == "hanging":
            continue
        if method == "initialize":
            send({
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "mock-mcp", "version": "0.1.0"},
                },
            })
        elif method == "tools/list":
            send({
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {
                    "tools": [{
                        "name": args.tool,
                        "description": "Echo the arguments back",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"text": {"type": "string"}},
                            "required": ["text"],
                        },
                    }],
                },
            })
        elif method == "tools/call":
            if args.mode == "slow":
                time.sleep(args.delay)
            if args.mode == "fail":
                send({
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "error": {"code": -32000, "message": "mock tool failure"},
                })
            else:
                arguments = ((message.get("params") or {}).get("arguments")) or {}
                send({
                    "jsonrpc": "2.0",
                    "id": request_id,
                    "result": {
                        "content": [{"type": "text", "text": f"echo:{json.dumps(arguments)}"}],
                        "isError": False,
                    },
                })
        elif method == "ping":
            send({"jsonrpc": "2.0", "id": request_id, "result": {}})
        else:
            send({
                "jsonrpc": "2.0",
                "id": request_id,
                "error": {"code": -32601, "message": f"method not found: {method}"},
            })
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
