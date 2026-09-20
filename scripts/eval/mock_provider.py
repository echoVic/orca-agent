#!/usr/bin/env python3
"""Fault-injection provider for Orca.

Serves an OpenAI-compatible ``POST /chat/completions`` SSE endpoint that can be
told to misbehave, so runtime/provider recovery paths can be exercised without
touching a real API. The scenario is selected per request by an ``FIMODE=<mode>``
token inside the prompt, so one server can drive a whole suite.

Modes:
  clean                  normal complete response
  truncate_once          HTTP-clean EOF with no SSE terminal marker (repro of #62)
  truncate_always        same, on every attempt
  content_truncate_once  stream reasoning + content, then HTTP-clean EOF
  drop_once              abrupt socket close mid-stream (transport error)
  http_429_once          one HTTP 429, then clean
  http_500_once          one HTTP 500, then clean
  empty_once             finish_reason stop with no content, then clean

Usage:
    python3 scripts/eval/mock_provider.py --port 8799 --log /tmp/fi-requests.jsonl
"""

from __future__ import annotations

import argparse
import json
import os
import re
import socket
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

USAGE = {"prompt_tokens": 101, "completion_tokens": 17, "prompt_cache_hit_tokens": 9}


def _chunk(payload: dict) -> bytes:
    return b"data: " + json.dumps(payload).encode() + b"\n\n"


def _delta(**fields: object) -> dict:
    choice: dict[str, object] = {"delta": fields, "finish_reason": None, "index": 0}
    return {"choices": [choice]}


class State:
    """Per-prompt attempt counters shared by all handler threads."""

    def __init__(self, log_path: str | None) -> None:
        self.lock = threading.Lock()
        self.attempts: dict[str, int] = {}
        self.seq = 0
        self.log_path = log_path

    def next_attempt(self, key: str) -> int:
        with self.lock:
            self.attempts[key] = self.attempts.get(key, 0) + 1
            return self.attempts[key]

    def record(self, entry: dict) -> None:
        with self.lock:
            self.seq += 1
            entry["seq"] = self.seq
            if self.log_path:
                with open(self.log_path, "a", encoding="utf-8") as fh:
                    fh.write(json.dumps(entry) + "\n")


def extract(payload: dict) -> tuple[str, str, str]:
    """Return (mode, attempt_key, prompt) for a request body."""
    prompt = ""
    for message in reversed(payload.get("messages") or []):
        if message.get("role") == "user":
            content = message.get("content")
            if isinstance(content, str):
                prompt = content
            elif isinstance(content, list):
                prompt = " ".join(
                    part.get("text", "") for part in content if isinstance(part, dict)
                )
            break
    mode = "clean"
    for token in prompt.replace("\n", " ").split():
        if token.startswith("FIMODE="):
            mode = token.split("=", 1)[1]
    key = f"{mode}:{hash(prompt) & 0xFFFFFFFF:08x}"
    return mode, key, prompt


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    state: State

    def log_message(self, *args: object) -> None:  # silence stderr chatter
        return

    # -- response helpers -------------------------------------------------
    def _sse_headers(self) -> None:
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()

    def _write_chunk(self, body: bytes) -> None:
        try:
            self.wfile.write(f"{len(body):X}\r\n".encode() + body + b"\r\n")
            self.wfile.flush()
        except (BrokenPipeError, ValueError):
            # The peer dropped the connection; that is the point of drop_once.
            self.close_connection = True

    def _end_chunks(self) -> None:
        try:
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()
        except (BrokenPipeError, ValueError):
            self.close_connection = True

    def _stream(
        self, *, reasoning: bool, content: bool, terminal: bool, usage: bool = False
    ) -> None:
        self._sse_headers()
        if reasoning:
            for index in range(3):
                self._write_chunk(
                    _chunk(
                        _delta(
                            role="assistant",
                            reasoning_content=f"reasoning step {index} for fault injection",
                        )
                    )
                )
                time.sleep(0.05)
        if content:
            self._write_chunk(_chunk(_delta(content="Fault-injection reply.")))
        if not terminal:
            if usage:
                # Real providers ship usage with include_usage even on long
                # streams; emitting it before the truncation lets the suite
                # check that a failed attempt is still accounted for.
                self._write_chunk(_chunk({"choices": [], "usage": USAGE}))
            # HTTP-clean EOF without the SSE terminal marker: this is the exact
            # shape of the #62 failure (stream ended before terminal marker).
            self._end_chunks()
            return
        self._write_chunk(_chunk({"choices": [{"delta": {}, "finish_reason": "stop", "index": 0}]}))
        self._write_chunk(_chunk({"choices": [], "usage": USAGE}))
        self._write_chunk(b"data: [DONE]\n\n")
        self._end_chunks()

    def _stream_tool_call_then_truncate(self) -> None:
        """Stream a complete tool call, then end without the terminal marker."""
        self._sse_headers()
        self._write_chunk(_chunk(_delta(role="assistant", reasoning_content="about to call a tool")))
        tool_call = {
            "choices": [
                {
                    "delta": {
                        "tool_calls": [
                            {
                                "index": 0,
                                "id": "call_fi_leak",
                                "type": "function",
                                "function": {
                                    "name": "bash",
                                    "arguments": json.dumps(
                                        {"command": "echo FAULT_INJECTION_TOOL_LEAK"}
                                    ),
                                },
                            }
                        ]
                    },
                    "finish_reason": None,
                    "index": 0,
                }
            ]
        }
        self._write_chunk(_chunk(tool_call))
        self._end_chunks()

    def _stream_tool_call(
        self, name: str, arguments: dict, call_id: str | None = None
    ) -> None:
        """Emit one complete tool call followed by a terminal marker.

        Real providers mint a fresh id per call; `call_id` lets a scenario reuse
        one on purpose (see the `dup_tool_id` fault mode).
        """
        self._sse_headers()
        self._write_chunk(
            _chunk(
                {
                    "choices": [
                        {
                            "delta": {
                                "tool_calls": [
                                    {
                                        "index": 0,
                                        "id": call_id
                                        or f"call_fi_{name}_{int(time.time() * 1000) % 1_000_000}",
                                        "type": "function",
                                        "function": {
                                            "name": name,
                                            "arguments": json.dumps(arguments),
                                        },
                                    }
                                ]
                            },
                            "finish_reason": None,
                            "index": 0,
                        }
                    ]
                }
            )
        )
        self._write_chunk(
            _chunk({"choices": [{"delta": {}, "finish_reason": "tool_calls", "index": 0}]})
        )
        self._write_chunk(_chunk({"choices": [], "usage": USAGE}))
        self._write_chunk(b"data: [DONE]\n\n")
        self._end_chunks()

    def _stream_tool_calls(self, calls: list[tuple[str, dict]]) -> None:
        """Emit several complete tool calls in one assistant turn."""
        self._sse_headers()
        tool_calls = []
        for index, (name, arguments) in enumerate(calls):
            tool_calls.append(
                {
                    "index": index,
                    "id": f"call_fi_{name}_{index}_{int(time.time() * 1000) % 1_000_000}",
                    "type": "function",
                    "function": {"name": name, "arguments": json.dumps(arguments)},
                }
            )
        self._write_chunk(_chunk({"choices": [{"delta": {"tool_calls": tool_calls},
                                                "finish_reason": None, "index": 0}]}))
        self._write_chunk(
            _chunk({"choices": [{"delta": {}, "finish_reason": "tool_calls", "index": 0}]})
        )
        self._write_chunk(_chunk({"choices": [], "usage": USAGE}))
        self._write_chunk(b"data: [DONE]\n\n")
        self._end_chunks()

    def _stream_bash_call(self, prompt: str) -> None:
        """Emit one bash tool call; arguments come from FISLEEP/FIYIELD/FIPty."""
        args: dict[str, object] = {}
        for token in prompt.replace("\n", " ").split():
            if token.startswith("FISLEEP="):
                args["command"] = f"sleep {token.split('=', 1)[1]}"
            elif token.startswith("FIYIELD="):
                args["yield_time_ms"] = int(token.split("=", 1)[1])
            elif token.startswith("FIPty="):
                args["terminal"] = "pty"
            elif token.startswith("FILIFETIME="):
                args["lifetime"] = token.split("=", 1)[1]
            elif token.startswith("FICMD64="):
                import base64

                try:
                    args["command"] = base64.b64decode(token.split("=", 1)[1]).decode()
                except Exception:  # noqa: BLE001 - keep the scenario running
                    pass
        args.setdefault("command", "sleep 1")
        self._stream_tool_call("bash", args)

    def _handle_tool_script(self, payload: dict, prompt: str) -> None:
        """Drive one scripted multi-step tool flow (FITOOL=<name>)."""
        tool = "bash"
        for token in prompt.replace("\n", " ").split():
            if token.startswith("FITOOL="):
                tool = token.split("=", 1)[1]
        results: list[dict] = []
        for message in payload.get("messages") or []:
            if message.get("role") != "tool":
                continue
            content = message.get("content")
            text = content if isinstance(content, str) else json.dumps(content)
            try:
                parsed = json.loads(text)
            except (json.JSONDecodeError, TypeError):
                parsed = {}
            results.append(parsed if isinstance(parsed, dict) else {})
        step = len(results)
        last = results[-1] if results else {}

        if tool == "timeout":
            if step == 0:
                self._stream_tool_call("bash", {"command": "sleep 30", "timeout_ms": 2000})
            else:
                self._stream(reasoning=False, content=True, terminal=True)
            return
        if tool == "stop":
            if step == 0:
                self._stream_tool_call("bash", {"command": "sleep 60", "yield_time_ms": 0})
            elif step == 1:
                self._stream_tool_call("task_stop", {"task_id": last.get("task_id") or "unknown"})
            else:
                self._stream(reasoning=False, content=True, terminal=True)
            return
        if tool == "interactive":
            if step == 0:
                self._stream_tool_call(
                    "bash",
                    {
                        "command": "printf 'name: '; read name; printf 'got:%s\\n' \"$name\"",
                        "terminal": "pty",
                        "yield_time_ms": 2000,
                    },
                )
            elif step == 1:
                self._stream_tool_call(
                    "task_send_input",
                    {"task_id": last.get("task_id") or "unknown", "chars": "hello\n"},
                )
            else:
                self._stream(reasoning=False, content=True, terminal=True)
            return
        if tool == "subagent":
            args: dict[str, object] = {
                "description": "probe child",
                "prompt": "Report the single word READY and nothing else.",
            }
            for token in prompt.replace("\n", " ").split():
                if token.startswith("FITYPE="):
                    args["subagent_type"] = token.split("=", 1)[1]
                elif token.startswith("FIMODEL="):
                    args["model"] = token.split("=", 1)[1]
                elif token.startswith("FIDEADLINE="):
                    args["deadline_ms"] = int(token.split("=", 1)[1])
                elif token.startswith("FISCHEMA="):
                    args["schema"] = {
                        "type": "object",
                        "properties": {"word": {"type": "string"}},
                        "required": ["word"],
                    }

            batch = 1
            for token in prompt.replace("\n", " ").split():
                if token.startswith("FIBATCH="):
                    batch = int(token.split("=", 1)[1])
            if step == 0:
                if batch > 1:
                    self._stream_tool_calls(
                        [("subagent", {**args, "description": f"probe child {i}"})
                         for i in range(batch)]
                    )
                else:
                    self._stream_tool_call("subagent", args)
            else:
                ids = [r.get("task_id") for r in results if r.get("task_id")]
                collected = any(
                    str(r.get("return_reason") or "").startswith("target_reached")
                    for r in results
                )
                if collected or (not ids and last.get("state") != "running"):
                    self._stream(reasoning=False, content=True, terminal=True)
                else:
                    self._stream_tool_call(
                        "task_wait",
                        {"task_ids": ids or ["unknown"], "wait_ms": 20000},
                    )
            return
        if tool == "ask":
            if step == 0:
                self._stream_tool_call(
                    "ask_user_question",
                    {
                        "questions": [
                            {
                                "header": "Probe",
                                "question": "Which branch should the probe use?",
                                "options": [
                                    {"label": "main", "description": "Use the main branch"},
                                    {"label": "release", "description": "Use the release branch"},
                                ],
                            }
                        ]
                    },
                )
            else:
                self._stream(reasoning=False, content=True, terminal=True)
            return
        if tool == "request_permissions":
            if step == 0:
                self._stream_tool_call(
                    "request_permissions",
                    {
                        "reason": "probe needs a network grant",
                        "permissions": {
                            "network": {"domains": {"host.docker.internal": "allow"}},
                            "fileSystem": {"write": ["/work/granted"]},
                        },
                    },
                )
            else:
                self._stream(reasoning=False, content=True, terminal=True)
            return
        if tool == "taskresume":
            task_id = "unknown"
            for token in prompt.replace("\n", " ").split():
                if token.startswith("FITASK="):
                    task_id = token.split("=", 1)[1]
            if step == 0:
                self._stream_tool_call("task_read_output", {"task_id": task_id})
            elif step == 1:
                self._stream_tool_call("task_wait", {"task_ids": [task_id], "wait_ms": 5000})
            else:
                self._stream(reasoning=False, content=True, terminal=True)
            return
        if tool == "mcpcall":
            tool_name = "mcp__probe__echo"
            for token in prompt.replace("\n", " ").split():
                if token.startswith("FIMCPTOOL="):
                    tool_name = token.split("=", 1)[1]
            if step == 0:
                self._stream_tool_call(tool_name, {"text": "hello from mcp"})
            else:
                self._stream(reasoning=False, content=True, terminal=True)
            return
        if tool == "cmd":
            # Run an arbitrary command supplied by the caller: FICMD64=<base64>.
            import base64

            command = "true"
            for token in prompt.replace("\n", " ").split():
                if token.startswith("FICMD64="):
                    try:
                        command = base64.b64decode(token.split("=", 1)[1]).decode()
                    except Exception:  # noqa: BLE001 - keep the scenario running
                        command = "true"
            if step == 0:
                self._stream_tool_call("bash", {"command": command})
            else:
                self._stream(reasoning=False, content=True, terminal=True)
            return
        if tool == "dup_tool_id":
            # Deliberately reuse one tool-call id for every call: providers are
            # supposed to mint unique ids, and the runtime's reaction to a
            # duplicate is what this scenario measures. Bounded so a recovered
            # session can finish: step 0 mints the id, step 1 reuses it in a
            # *later turn* (the reported repro), step 2 completes.
            if step < 2:
                self._stream_tool_call(
                    "bash", {"command": "true"}, call_id="call_fi_duplicate"
                )
            else:
                self._stream(reasoning=False, content=True, terminal=True)
            return
        if tool == "filler":
            limit = 30
            for token in prompt.replace("\n", " ").split():
                if token.startswith("FIFILL="):
                    limit = int(token.split("=", 1)[1])
            if step < limit:
                self._stream_tool_call(
                    "bash", {"command": "seq 1 20000", "max_output_tokens": 20000}
                )
            else:
                self._stream(reasoning=False, content=True, terminal=True)
            return
        if tool == "flood":
            if step == 0:
                self._stream_tool_call("bash", {"command": "yes", "yield_time_ms": 0})
            elif step == 1:
                self._stream_tool_call("task_stop", {"task_id": last.get("task_id") or "unknown"})
            else:
                self._stream(reasoning=False, content=True, terminal=True)
            return
        if tool == "orphan":
            if step == 0:
                marker = f"FI_ORPHAN_{int(time.time())}"
                command = (
                    f"i=0; while [ $i -lt 40 ]; do echo {marker}-$i >> /tmp/fi-orphan-ticks;"
                    " i=$((i+1)); sleep 1; done"
                )
                self._stream_tool_call(
                    "bash", {"command": command, "yield_time_ms": 0}
                )
            else:
                self._stream(reasoning=False, content=True, terminal=True)
            return
        if tool == "bigout":
            if step == 0:
                self._stream_tool_call("bash", {"command": "seq 1 400000"})
            else:
                self._stream(reasoning=False, content=True, terminal=True)
            return
        # default: run the requested sleep command and poll it to completion
        if step == 0:
            self._stream_bash_call(prompt)
        elif last.get("state") == "running":
            self._stream_tool_call("task_wait", {"task_ids": [last.get("task_id") or "unknown"]})
        else:
            self._stream(reasoning=False, content=True, terminal=True)

    def _json_error(self, status: int) -> None:
        body = json.dumps({"error": {"message": f"injected HTTP {status}", "type": "fi"}}).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(body)

    # -- request handling -------------------------------------------------
    def do_POST(self) -> None:  # noqa: N802 - http.server API
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b"{}"
        try:
            payload = json.loads(raw)
        except json.JSONDecodeError:
            payload = {}
        mode, key, prompt = extract(payload)
        attempt = self.state.next_attempt(key)
        watch = [t for t in os.environ.get("MOCK_WATCH", "").split(",") if t]
        self.state.record(
            {
                "watch": {token: token in raw.decode(errors="replace") for token in watch},
                "mode": mode,
                "attempt": attempt,
                "path": self.path,
                "model": payload.get("model"),
                "reasoning_effort": payload.get("reasoning_effort"),
                "messages": len(payload.get("messages") or []),
                "tools": len(payload.get("tools") or []),
                "prompt": prompt[:160],
                "prompt_len": len(prompt),
                "prompt_sha256": __import__("hashlib").sha256(prompt.encode()).hexdigest(),
                "ts": time.time(),
            }
        )

        first = attempt == 1
        has_tool_result = any(
            message.get("role") == "tool" for message in payload.get("messages") or []
        )
        if mode in ("tool_bash", "tool_script"):
            self._handle_tool_script(payload, prompt)
            return
        if mode == "http_429_once" and first:
            self._json_error(429)
            return
        if mode == "http_500_once" and first:
            self._json_error(500)
            return
        if mode == "http_522_once" and first:
            # Cloudflare "connection timed out"; outside the classifier's
            # ["500","502","503","504"] allowlist.
            self._json_error(522)
            return
        if mode == "empty_once" and first:
            self._sse_headers()
            self._write_chunk(
                _chunk({"choices": [{"delta": {}, "finish_reason": "stop", "index": 0}]})
            )
            self._write_chunk(_chunk({"choices": [], "usage": USAGE}))
            self._write_chunk(b"data: [DONE]\n\n")
            self._end_chunks()
            return
        if mode == "stall_once" and first:
            # One chunk, then silence past the 300 s streaming idle read budget.
            self._sse_headers()
            self._write_chunk(_chunk(_delta(role="assistant", reasoning_content="stalling")))
            time.sleep(320)
            self._end_chunks()
            return
        if mode == "drop_once" and first:
            self._sse_headers()
            self._write_chunk(_chunk(_delta(role="assistant", reasoning_content="partial")))
            self.close_connection = True
            try:
                self.connection.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            self.connection.close()
            return
        if mode == "truncate_always":
            self._stream(reasoning=True, content=False, terminal=False, usage=True)
            return
        if mode == "truncate_silent_always":
            self._stream(reasoning=True, content=False, terminal=False)
            return
        if mode == "truncate_once" and first:
            self._stream(reasoning=True, content=False, terminal=False, usage=True)
            return
        if mode == "content_truncate_once" and first:
            self._stream(reasoning=True, content=True, terminal=False)
            return
        if mode == "toolcall_truncate_once" and first:
            self._stream_tool_call_then_truncate()
            return
        self._stream(reasoning=True, content=True, terminal=True)


def serve(port: int, log_path: str | None, host: str = "127.0.0.1") -> ThreadingHTTPServer:
    state = State(log_path)

    class Bound(Handler):
        pass

    Bound.state = state
    server = ThreadingHTTPServer((host, port), Bound)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, default=8799)
    parser.add_argument("--log", default=None)
    parser.add_argument("--host", default="127.0.0.1", help="bind address; use 0.0.0.0 for containers")
    args = parser.parse_args()
    server = serve(args.port, args.log, args.host)
    print(f"fault-injection provider listening on http://{args.host}:{args.port}", flush=True)
    try:
        while True:
            time.sleep(3600)
    except KeyboardInterrupt:
        server.shutdown()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
