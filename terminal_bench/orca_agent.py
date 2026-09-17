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
import time
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

#: Provider knobs forwarded into the container. `reasoning_effort` is the model
#: latency lever (issue #64): TB2 task budgets are fixed at 900-3600 s and the
#: median trial spends ~60 % of its wall clock inside model generation, so a
#: benchmark arm has to be able to A/B `low|high|max` under one budget.
PROVIDER_ENV = (
    "ORCA_REASONING_EFFORT",
    "DEEPSEEK_REASONING_EFFORT",
    "ORCA_PROVIDER",
)

#: Usage counters reported in `execution_metadata.json` for post-mortems.
USAGE_FIELDS = ("turns", "output_tokens", "input_tokens", "cache_tokens", "cost_usd_micros")


def _load_api_key() -> str:
    """Read DEEPSEEK_API_KEY from ~/.orca/auth.json, fall back to env."""
    auth_file = Path.home() / ".orca" / "auth.json"
    if auth_file.exists():
        data = json.loads(auth_file.read_text())
        if key := data.get("DEEPSEEK_API_KEY"):
            return key
    return os.environ.get("ORCA_API_KEY", "")


def _usage_summary(events: list[dict]) -> dict:
    """Largest usage counters seen anywhere in the stream.

    A trial killed at the task timeout never emits `session.completed`, but the
    task status events carry the running totals, so a post-mortem can still tell
    "the task needed more time" from "the agent burned the budget" without
    replaying the trajectory by hand (issue #64).
    """
    totals = dict.fromkeys(USAGE_FIELDS)
    stack: list[object] = [event.get("payload") for event in events]
    while stack:
        node = stack.pop()
        if isinstance(node, dict):
            for key, value in node.items():
                if (
                    key in totals
                    and isinstance(value, int)
                    and not isinstance(value, bool)
                ):
                    current = totals[key]
                    totals[key] = value if current is None else max(current, value)
                elif isinstance(value, (dict, list)):
                    stack.append(value)
        elif isinstance(node, list):
            stack.extend(node)
    return totals


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
        await self.exec_as_root(
            environment,
            command=(
                "apt-get update && apt-get install -y git ripgrep"
                " && cp /mnt/orca-bin/orca /usr/local/bin/orca"
                " && chmod +x /usr/local/bin/orca"
            ),
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
        # The provider knobs are only settable through the environment, so a
        # benchmark arm cannot vary them without this forwarding (issue #64).
        for var in PROVIDER_ENV:
            if value := os.environ.get(var):
                env[var] = value

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
            "provider": {var: os.environ.get(var) for var in PROVIDER_ENV if var in env},
            "duration_seconds": None,
            "exit_code": None,
            "terminal": None,
            "trajectory_persisted": True,
            "usage": None,
            "verifier_result": None,
        }
        result = None
        started = time.monotonic()
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
            metadata["duration_seconds"] = round(time.monotonic() - started, 3)
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
                metadata["usage"] = _usage_summary(events)
            except json.JSONDecodeError:
                metadata["terminal"] = {"status": None, "terminal": None}
            (logs_dir / "execution_metadata.json").write_text(
                json.dumps(metadata, indent=2),
                encoding="utf-8",
            )

    def populate_context_post_run(self, context: AgentContext) -> None:
        pass
