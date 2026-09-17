"""
Harbor adapter for running Orca (blade-deepseek) on Terminal-Bench.

Usage:
    harbor run -d "terminal-bench/terminal-bench-2" \
        --agent "terminal_bench.orca_agent:OrcaInstalledAgent" \
        -k 5
"""

import json
import os
import shlex
import subprocess
from pathlib import Path

from harbor.agents.installed.base import BaseInstalledAgent, with_prompt_template
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext

ORCA_LOCAL_MUSL_BIN = str(
    Path(__file__).resolve().parent.parent
    / "target/x86_64-unknown-linux-musl/release/orca"
)

#: Env vars controlling the execution budget; unset means unlimited.
BUDGET_ENV = {
    "max-turns": "ORCA_MAX_TURNS",
    "max-tool-calls": "ORCA_MAX_TOOL_CALLS",
    "max-cost-usd": "ORCA_MAX_COST_USD",
    "max-wall-time-secs": "ORCA_MAX_WALL_TIME_SECS",
}

#: Budget for the install step. Harbor's agent-setup timeout defaults to a fixed
#: 360 s, which an `apt-get update` on a slow image used to exhaust before Orca
#: ever started (issue #60); the adapter asks Harbor for more and overrides the
#: per-exec timeout, and `--agent-setup-timeout-multiplier` remains available for
#: images that need even longer.
DEFAULT_SETUP_TIMEOUT_SEC = 900


def _load_api_key() -> str:
    """Read DEEPSEEK_API_KEY from ~/.orca/auth.json, fall back to env."""
    auth_file = Path.home() / ".orca" / "auth.json"
    if auth_file.exists():
        data = json.loads(auth_file.read_text())
        if key := data.get("DEEPSEEK_API_KEY"):
            return key
    return os.environ.get("ORCA_API_KEY", "")


def _terminal_summary(events: list[dict]) -> dict:
    """Extract terminal metadata from the streamed JSONL projection.

    The terminal is the typed object on the final `session.completed` event;
    adapters never reconstruct budget facts from constants.
    """
    for event in reversed(events):
        if event.get("type") != "session.completed":
            continue
        payload = event.get("payload", {})
        return {
            "status": payload.get("status"),
            "terminal": payload.get("terminal"),
            "session_id": payload.get("session_id"),
        }
    return {"status": None, "terminal": None, "session_id": None}


class OrcaInstalledAgent(BaseInstalledAgent):
    """Orca coding agent adapter for Harbor / Terminal-Bench."""

    def __init__(
        self,
        *args,
        override_setup_timeout_sec: int | float | None = None,
        **kwargs,
    ):
        """Accept Harbor's setup-budget override (and an env equivalent)."""
        self._install_exec_timeout_sec = int(
            override_setup_timeout_sec
            or os.environ.get("ORCA_AGENT_SETUP_TIMEOUT_SEC")
            or DEFAULT_SETUP_TIMEOUT_SEC
        )
        super().__init__(*args, **kwargs)

    @staticmethod
    def name() -> str:
        return "orca"

    def version(self) -> str | None:
        try:
            result = subprocess.run(
                [ORCA_LOCAL_MUSL_BIN, "--version"],
                capture_output=True,
                check=True,
                text=True,
            )
        except (OSError, subprocess.CalledProcessError):
            return None
        parts = result.stdout.strip().split()
        return parts[1] if len(parts) >= 2 else None

    async def install(self, environment: BaseEnvironment) -> None:
        # The mounted binary is the only hard requirement, so it is copied in a
        # step of its own: a slow package mirror can then never leave the trial
        # without an `orca` to run.
        await self.exec_as_root(
            environment,
            command=(
                "cp /mnt/orca-bin/orca /usr/local/bin/orca"
                " && chmod +x /usr/local/bin/orca"
            ),
            timeout_sec=self._install_exec_timeout_sec,
        )
        # `git` and `ripgrep` are conveniences, not prerequisites: Orca's own
        # tools do not need them and the agent can install whatever a task
        # requires through its own shell tool, whose budget is far larger than
        # Harbor's fixed 360 s setup budget. Provision them best-effort — skip
        # when present, retry the index, and never fail the setup step — because
        # the unconditional `apt-get update` used to abort trials on slow images
        # (issue #60) and an interrupted dpkg leaves the verifier's own install
        # broken (issue #65).
        await self.exec_as_root(
            environment,
            command=(
                "export DEBIAN_FRONTEND=noninteractive; "
                "if command -v git >/dev/null 2>&1 && command -v rg >/dev/null 2>&1; "
                "then echo 'orca-adapter: git and ripgrep already present'; exit 0; fi; "
                "apt-get update -o Acquire::Retries=5 "
                "|| echo 'orca-adapter: apt-get update failed, continuing without it'; "
                "apt-get install -y --no-install-recommends -o Acquire::Retries=5 git ripgrep "
                "|| echo 'orca-adapter: git/ripgrep install failed, continuing'; "
                "exit 0"
            ),
            timeout_sec=self._install_exec_timeout_sec,
        )

    @with_prompt_template
    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        env = {
            "DEEPSEEK_API_KEY": _load_api_key(),
            "ORCA_BASE_URL": os.environ.get("ORCA_BASE_URL", "https://api.deepseek.com"),
            "ORCA_MODEL": os.environ.get("ORCA_MODEL", "deepseek-flash"),
        }

        budget_flags = []
        for arg, var in BUDGET_ENV.items():
            if (value := os.environ.get(var)) is not None:
                budget_flags.append(f" --{arg} {shlex.quote(value)}")

        cmd = (
            f"orca exec"
            f" --mode full-auto"
            f" --output-format jsonl"
            f"{''.join(budget_flags)}"
            f" {shlex.quote(instruction)}"
        )

        logs_dir = Path(self.logs_dir)
        logs_dir.mkdir(parents=True, exist_ok=True)
        metadata = {
            "binary": self.version() or "unknown",
            "budget": {
                key: os.environ.get(var)
                for key, var in BUDGET_ENV.items()
                if os.environ.get(var) is not None
            },
            "exit_code": None,
            "terminal": None,
            "trajectory_persisted": True,
            "verifier_result": None,
        }
        result = None
        try:
            result = await self.exec_as_agent(environment, command=cmd, env=env)
            if result is not None:
                metadata["exit_code"] = getattr(result, "exit_code", None)
        except Exception as error:  # noqa: BLE001 - persist everything on failure
            metadata["error"] = str(error)
            raise
        finally:
            # Always persist stdout, stderr, exit code, terminal metadata, and
            # the raw trajectory on every exit path (including non-zero exits).
            output = result.stdout if result is not None else ""
            stderr = result.stderr if result is not None else ""
            (logs_dir / "trajectory.jsonl").write_text(output, encoding="utf-8")
            (logs_dir / "stderr.txt").write_text(stderr or "", encoding="utf-8")
            try:
                events = [
                    json.loads(line)
                    for line in output.splitlines()
                    if line.strip().startswith("{")
                ]
                metadata["terminal"] = _terminal_summary(events)
            except json.JSONDecodeError:
                metadata["terminal"] = {"status": None, "terminal": None}
            (logs_dir / "execution_metadata.json").write_text(
                json.dumps(metadata, indent=2),
                encoding="utf-8",
            )

    def populate_context_post_run(self, context: AgentContext) -> None:
        pass
