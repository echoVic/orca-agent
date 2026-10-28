"""Shared TUI driver for the TUI-only probes (goal mode, workflow mode).

A probe gets a private tmux server, a temporary `ORCA_HOME` with a trusted workspace, the
scripted HTTP provider, and a launcher that points the TUI at it. The driver handles the
first-run panel (Welcome / trust / doctor) and exposes the pane plus liveness, so the probe
can drive `/goal`, `/workflows` and the like exactly as a user would.
"""

from __future__ import annotations

import json
import os
import shlex
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parent.parent


class Tmux:
    """A private tmux server: a probe never touches the user's sessions."""

    def __init__(self, socket_name: str) -> None:
        self.socket_name = socket_name
        self.owned = False

    def call(self, *args, check: bool = True) -> str:
        result = subprocess.run(
            ["tmux", "-L", self.socket_name, "-f", "/dev/null", *map(str, args)],
            capture_output=True,
            text=True,
            timeout=20,
        )
        if check and result.returncode:
            raise RuntimeError(f"tmux {args[0]} failed: {result.stderr.strip()}")
        return result.stdout.strip()

    def start(self) -> None:
        if self.call("list-sessions", check=False):
            raise RuntimeError(f"tmux -L {self.socket_name} is in use; leave it intact")
        self.call("new-session", "-d", "-s", "eval", "-n", "control", "-x", "220", "-y", "60", "exec cat")
        self.call("set-option", "-g", "remain-on-exit", "on")
        self.call("set-option", "-g", "history-limit", "5000")
        self.owned = True

    def window(self, name: str, argv: list[str], extra_env: dict[str, str] | None = None) -> str:
        env_args: list[str] = []
        for key, value in (extra_env or {}).items():
            env_args += ["-e", f"{key}={value}"]
        self.call("new-window", "-d", "-t", "eval:", "-n", name, *env_args, "exec " + shlex.join(map(str, argv)))
        return f"eval:{name}.0"

    def alive(self, pane: str) -> bool:
        return self.call("display-message", "-p", "-t", pane, "#{pane_dead}", check=False) == "0"

    def stop(self) -> None:
        if self.owned:
            self.call("kill-server", check=False)
            self.owned = False


class TuiFixture:
    """Temporary ORCA_HOME, trusted workspace, scripted provider and the TUI launcher."""

    def __init__(self, binary: Path, out: Path, files: dict[str, str] | None = None) -> None:
        self.binary = binary
        self.out = out
        self.base = Path(tempfile.mkdtemp(prefix="orca-tui-fixture."))
        self.home = self.base / "home"
        self.ws = self.base / "ws"
        self.home.mkdir(parents=True)
        self.ws.mkdir(parents=True)
        (self.home / "config.toml").write_text("auto_memory = false\n", encoding="utf-8")
        (self.ws / "README.md").write_text("probe workspace\n", encoding="utf-8")
        for relative, content in (files or {}).items():
            target = self.ws / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(content, encoding="utf-8")
        subprocess.run(["git", "init", "-q", str(self.ws)], check=True, capture_output=True)
        self.env = {
            key: value
            for key, value in os.environ.items()
            if not key.startswith(("ORCA_", "DEEPSEEK_", "OPENAI_", "ANTHROPIC_")) and key != "NODE_OPTIONS"
        }
        self.env.update(ORCA_HOME=str(self.home), TERM="xterm-256color", DEEPSEEK_API_KEY="mock")
        subprocess.run(
            [str(self.binary), "trust", "add", "--cwd", str(self.ws)],
            env=self.env,
            cwd=self.ws,
            check=True,
            capture_output=True,
        )
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            self.port = int(sock.getsockname()[1])
        self.request_log = self.out / "requests.jsonl"
        self.provider = subprocess.Popen(
            [sys.executable, str(HERE / "mock_provider.py"), "--port", str(self.port), "--log", str(self.request_log)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            try:
                with socket.create_connection(("127.0.0.1", self.port), timeout=0.2):
                    break
            except OSError:
                time.sleep(0.1)
        self.launcher = self.base / "launch.py"
        self.launcher.write_text(
            "import os, shlex\n"
            f"env = dict(os.environ); env.update(ORCA_HOME={str(self.home)!r}, TERM='xterm-256color', DEEPSEEK_API_KEY='mock')\n"
            f"os.chdir({str(self.ws)!r})\n"
            f"argv = [{str(self.binary)!r}, '--provider', 'deepseek', '--base-url', 'http://127.0.0.1:{self.port}',"
            " '--api-key', 'mock', '--model', 'auto', '--mode', 'auto-edit',"
            f" '--cwd', {str(self.ws)!r}] + shlex.split(os.environ.get('ORCA_TUI_EXTRA', ''))\n"
            "os.execve(argv[0], argv, env)\n",
            encoding="utf-8",
        )

    def sessions(self) -> list[Path]:
        root = self.home / "sessions"
        return sorted(root.rglob("*.jsonl")) if root.exists() else []

    def session_text(self) -> str:
        return "".join(path.read_text(encoding="utf-8", errors="replace") for path in self.sessions())

    def workflow_runs(self) -> list[dict]:
        """Parsed `orca workflow list` (JSON) restricted to this workspace's runs."""
        result = subprocess.run(
            [str(self.binary), "workflow", "list"],
            env=self.env,
            cwd=self.ws,
            capture_output=True,
            text=True,
            timeout=30,
        )
        try:
            rows = json.loads(result.stdout.strip() or "[]")
        except json.JSONDecodeError:
            return []
        if not isinstance(rows, list):
            return []
        return [row for row in rows if isinstance(row, dict) and row.get("cwd") == str(self.ws)]

    def close(self) -> None:
        self.provider.kill()


class Tui:
    def __init__(self, tmux: Tmux, fixture: TuiFixture) -> None:
        self.tmux, self.fixture = tmux, fixture
        self.pane: str | None = None

    def start(self, extra_args: list[str] | None = None, wait_ready: bool = True) -> None:
        argv = [sys.executable, "-I", str(self.fixture.launcher)]
        self.pane = self.tmux.window("orca", argv, extra_env={"ORCA_TUI_EXTRA": " ".join(extra_args or [])})
        if wait_ready:
            self.wait_ready()

    def wait_ready(self, timeout: float = 60) -> None:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            pane = self.capture()
            if not self.tmux.alive(self.pane or ""):
                raise RuntimeError(f"TUI exited during startup:\n{pane[-1500:]}")
            if any(token in pane for token in ("Trust workspace", "Continue untrusted", "╭ Welcome", "E  Exit")):
                self.tmux.call("send-keys", "-t", self.pane, "Enter")
                time.sleep(1.2)
                continue
            if "Message Orca" in pane:
                return
            time.sleep(0.3)
        raise RuntimeError(f"TUI did not become ready:\n{self.capture()[-1500:]}")

    def capture(self) -> str:
        return self.tmux.call("capture-pane", "-p", "-t", self.pane) if self.pane else ""

    def alive(self) -> bool:
        return self.tmux.alive(self.pane or "")

    def send(self, text: str, pause: float = 0.0) -> None:
        self.tmux.call("send-keys", "-t", self.pane, "-l", text)
        self.tmux.call("send-keys", "-t", self.pane, "Enter")
        if pause:
            time.sleep(pause)

    def kill(self) -> None:
        if self.pane:
            self.tmux.call("kill-window", "-t", self.pane, check=False)
            self.pane = None


def wait_for(predicate, timeout: float, interval: float = 0.25):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(interval)
    return None


def resolve_out(repo_out: str | Path) -> Path:
    path = Path(repo_out)
    if not path.is_absolute():
        path = REPO / path
    stamp = time.strftime("%Y%m%d-%H%M%S")
    target = path / stamp
    target.mkdir(parents=True, exist_ok=True)
    return target


def json_dump(path: Path, payload: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")
