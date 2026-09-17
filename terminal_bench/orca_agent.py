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

from harbor.agents.installed.base import (
    BaseInstalledAgent,
    NonZeroAgentExitCodeError,
    with_prompt_template,
)
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

        budget_flags = []
        for arg, var in BUDGET_ENV.items():
            if (value := os.environ.get(var)) is not None:
                budget_flags.append(f" --{arg} {shlex.quote(value)}")

        # The stream is teed inside the container as well: Harbor discards the
        # `ExecResult` when the command exits non-zero *and* when the exec is
        # killed at the task timeout, so the in-container copy is the only
        # trajectory that survives both paths (issue #59).
        trajectory_in_container = "/tmp/orca-trajectory.jsonl"
        cmd = (
            f"orca exec"
            f" --mode full-auto"
            f" --output-format jsonl"
            f"{''.join(budget_flags)}"
            f" {shlex.quote(instruction)}"
            f" 2>&1 | tee {trajectory_in_container}"
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
            "trajectory_bytes": 0,
            "trajectory_persisted": False,
            "verifier_result": None,
        }
        result = None
        failure = None
        try:
            # `environment.exec` rather than `exec_as_agent`: the helper raises
            # `NonZeroAgentExitCodeError` and throws the `ExecResult` away, so
            # the output of a crashed or budget-stopped run never reaches the
            # `finally` block that persists it. The command still runs as the
            # agent user (`user=None` resolves to the environment default) and
            # keeps `pipefail`, so the exit code stays orca's, not `tee`'s.
            result = await environment.exec(
                command=f"set -o pipefail; {cmd}",
                env=env,
            )
        except Exception as error:  # noqa: BLE001 - persist everything on failure
            failure = error
            metadata["error"] = str(error)
        finally:
            # Always persist stdout, stderr, exit code, terminal metadata, and
            # the raw trajectory on every exit path (including non-zero exits
            # and timeouts, where `result` is None).
            output = result.stdout if result is not None else ""
            stderr = result.stderr if result is not None else ""
            if result is not None:
                metadata["exit_code"] = result.return_code
            persisted = 0
            try:
                await environment.download_file(
                    trajectory_in_container, logs_dir / "trajectory.jsonl"
                )
                persisted = (logs_dir / "trajectory.jsonl").stat().st_size
            except Exception as error:  # noqa: BLE001 - evidence is best effort
                metadata["trajectory_download_error"] = str(error)
            if not persisted:
                (logs_dir / "trajectory.jsonl").write_text(output, encoding="utf-8")
                persisted = len(output.encode("utf-8"))
            metadata["trajectory_bytes"] = persisted
            metadata["trajectory_persisted"] = persisted > 0
            (logs_dir / "stderr.txt").write_text(stderr or "", encoding="utf-8")
            try:
                # Parse the persisted trajectory, not `output`: it is the copy
                # that also exists when the exec result was discarded.
                stream = (logs_dir / "trajectory.jsonl").read_text(
                    encoding="utf-8", errors="replace"
                )
                events = [
                    json.loads(line)
                    for line in stream.splitlines()
                    if line.strip().startswith("{")
                ]
                metadata["terminal"] = _terminal_summary(events)
            except json.JSONDecodeError:
                metadata["terminal"] = {"status": None, "terminal": None}
            (logs_dir / "execution_metadata.json").write_text(
                json.dumps(metadata, indent=2),
                encoding="utf-8",
            )
        if failure is not None:
            raise failure
        if result is not None and result.return_code != 0:
            # Raise only after the evidence is on disk, and with Harbor's own
            # error type so the trial is still classified as a failed agent run.
            raise NonZeroAgentExitCodeError(
                f"Command failed (exit {result.return_code}): {cmd}"
            )

    def populate_context_post_run(self, context: AgentContext) -> None:
        pass
