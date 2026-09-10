"""
External (non-installed) Harbor adapter for Orca.

Use this when Orca is already built and available on PATH,
or when you want to point to a local debug build.

Usage:
    harbor run -d "terminal-bench/terminal-bench-2" \
        --agent-import-path "orca_external:OrcaExternalAgent" \
        -k 5

Environment variables:
    ORCA_BIN        Path to orca binary (default: "orca")
    ORCA_API_KEY    DeepSeek API key
    ORCA_BASE_URL   API base URL (default: https://api.deepseek.com)
    ORCA_MODEL      Model name (default: deepseek-flash)
"""

import json
import os
import shlex
from pathlib import Path

from harbor.agents.base import BaseAgent
from harbor.environments.base import BaseEnvironment
from harbor.models.agent.context import AgentContext


def _load_api_key() -> str:
    """Read DEEPSEEK_API_KEY from ~/.orca/auth.json, fall back to env."""
    auth_file = Path.home() / ".orca" / "auth.json"
    if auth_file.exists():
        data = json.loads(auth_file.read_text())
        if key := data.get("DEEPSEEK_API_KEY"):
            return key
    return os.environ.get("ORCA_API_KEY", "")


class OrcaExternalAgent(BaseAgent):
    """Runs Orca headlessly via exec against the Harbor environment."""

    @staticmethod
    def name() -> str:
        return "orca"

    def version(self) -> str | None:
        return None

    async def setup(self, environment: BaseEnvironment) -> None:
        pass

    async def run(
        self,
        instruction: str,
        environment: BaseEnvironment,
        context: AgentContext,
    ) -> None:
        orca_bin = os.environ.get("ORCA_BIN", "orca")

        cmd = (
            f"DEEPSEEK_API_KEY={shlex.quote(_load_api_key())}"
            f" {shlex.quote(orca_bin)} exec"
            f" --mode full-auto"
            f" --output-format jsonl"
            f" {shlex.quote(instruction)}"
        )

        result = await environment.exec(cmd)
