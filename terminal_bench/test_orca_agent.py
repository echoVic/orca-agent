import asyncio
import json
import os
import sys
import tempfile
import types
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import AsyncMock, patch


def _install_harbor_stubs() -> None:
    modules = {
        "harbor": types.ModuleType("harbor"),
        "harbor.agents": types.ModuleType("harbor.agents"),
        "harbor.agents.base": types.ModuleType("harbor.agents.base"),
        "harbor.agents.installed": types.ModuleType("harbor.agents.installed"),
        "harbor.agents.installed.base": types.ModuleType("harbor.agents.installed.base"),
        "harbor.environments": types.ModuleType("harbor.environments"),
        "harbor.environments.base": types.ModuleType("harbor.environments.base"),
        "harbor.models": types.ModuleType("harbor.models"),
        "harbor.models.agent": types.ModuleType("harbor.models.agent"),
        "harbor.models.agent.context": types.ModuleType("harbor.models.agent.context"),
    }

    class BaseInstalledAgent:
        pass

    class BaseAgent:
        pass

    class BaseEnvironment:
        pass

    class AgentContext:
        __slots__ = (
            "n_input_tokens",
            "n_cache_tokens",
            "n_output_tokens",
            "cost_usd",
            "rollout_details",
            "metadata",
        )

    modules["harbor.agents.base"].BaseAgent = BaseAgent
    modules["harbor.agents.installed.base"].BaseInstalledAgent = BaseInstalledAgent
    modules["harbor.agents.installed.base"].with_prompt_template = lambda fn: fn
    modules["harbor.environments.base"].BaseEnvironment = BaseEnvironment
    modules["harbor.models.agent.context"].AgentContext = AgentContext
    sys.modules.update(modules)


_install_harbor_stubs()

from terminal_bench import orca_agent, orca_external


class OrcaInstalledAgentTests(unittest.TestCase):
    @patch("terminal_bench.orca_agent.subprocess.run")
    def test_version_is_derived_from_mounted_binary(self, run) -> None:
        run.return_value = SimpleNamespace(stdout="orca 0.3.4\n")

        self.assertEqual(orca_agent.OrcaInstalledAgent().version(), "0.3.4")
        run.assert_called_once_with(
            [orca_agent.ORCA_LOCAL_MUSL_BIN, "--version"],
            capture_output=True,
            check=True,
            text=True,
        )

    def test_run_persists_trajectory_without_extending_context(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            agent = orca_agent.OrcaInstalledAgent()
            agent.logs_dir = Path(directory)
            agent.exec_as_agent = AsyncMock(
                return_value=SimpleNamespace(stdout='{"type":"turn.completed"}\n', stderr="")
            )
            context = orca_agent.AgentContext()

            asyncio.run(agent.run("finish the task", SimpleNamespace(), context))

            expected = '{"type":"turn.completed"}\n'
            self.assertFalse(hasattr(context, "output"))
            self.assertEqual(
                (Path(directory) / "trajectory.jsonl").read_text(encoding="utf-8"),
                expected,
            )
            metadata = json.loads(
                (Path(directory) / "execution_metadata.json").read_text(
                    encoding="utf-8"
                )
            )
            self.assertEqual(metadata["budget"], {})
            self.assertEqual(metadata["terminal"]["status"], None)

    def test_run_persists_terminal_metadata_and_stderr_on_nonzero_exit(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            agent = orca_agent.OrcaInstalledAgent()
            agent.logs_dir = Path(directory)
            agent.exec_as_agent = AsyncMock(
                return_value=SimpleNamespace(
                    exit_code=4,
                    stdout=(
                        '{"type":"session.started","seq":0}\n'
                        '{"type":"session.completed","payload":{"status":"budget_exhausted",'
                        '"terminal":{"stopped":{"reason":{"turn_budget":{"max_turns":3}},'
                        '"usage":{"turns":3,"tool_calls":0,"cost_usd_micros":0,'
                        '"wall_time_ms":0},"checkpoint_id":"cp-1","resumable":true}}}}\n'
                    ),
                    stderr="headless budget stop\n",
                )
            )
            context = orca_agent.AgentContext()

            asyncio.run(agent.run("finish the task", SimpleNamespace(), context))

            metadata = json.loads(
                (Path(directory) / "execution_metadata.json").read_text(
                    encoding="utf-8"
                )
            )
            self.assertEqual(metadata["exit_code"], 4)
            terminal = metadata["terminal"]
            self.assertEqual(terminal["status"], "budget_exhausted")
            self.assertEqual(
                terminal["terminal"]["stopped"]["reason"]["turn_budget"][
                    "max_turns"
                ],
                3,
            )
            self.assertTrue(metadata["trajectory_persisted"])
            self.assertEqual(
                (Path(directory) / "stderr.txt").read_text(encoding="utf-8"),
                "headless budget stop\n",
            )

    def test_run_persists_metadata_even_when_execution_raises(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            agent = orca_agent.OrcaInstalledAgent()
            agent.logs_dir = Path(directory)
            agent.exec_as_agent = AsyncMock(side_effect=RuntimeError("agent crashed"))
            context = orca_agent.AgentContext()

            with self.assertRaises(RuntimeError):
                asyncio.run(agent.run("finish the task", SimpleNamespace(), context))

            metadata = json.loads(
                (Path(directory) / "execution_metadata.json").read_text(
                    encoding="utf-8"
                )
            )
            self.assertEqual(metadata["exit_code"], None)
            self.assertIn("error", metadata)
            self.assertTrue(metadata["trajectory_persisted"])
            self.assertEqual(
                (Path(directory) / "trajectory.jsonl").read_text(encoding="utf-8"),
                "",
            )

    def test_run_forwards_explicit_budget_flags_from_environment(self) -> None:
        import importlib

        with tempfile.TemporaryDirectory() as directory:
            agent = orca_agent.OrcaInstalledAgent()
            agent.logs_dir = Path(directory)
            captured = {}

            async def fake_exec(environment, command, env):
                captured["command"] = command
                return SimpleNamespace(stdout="", stderr="")

            agent.exec_as_agent = fake_exec
            with patch.dict(
                os.environ,
                {"ORCA_MAX_TURNS": "5", "ORCA_MAX_COST_USD": "0.5"},
            ):
                importlib.reload(orca_agent)
                agent_2 = orca_agent.OrcaInstalledAgent()
                agent_2.logs_dir = Path(directory)
                agent_2.exec_as_agent = fake_exec
                asyncio.run(
                    agent_2.run("finish the task", SimpleNamespace(), orca_agent.AgentContext())
                )

            self.assertIn("--max-turns 5", captured["command"])
            self.assertIn("--max-cost-usd 0.5", captured["command"])

    def test_external_run_does_not_extend_context(self) -> None:
        environment = SimpleNamespace(
            exec=AsyncMock(return_value=SimpleNamespace(stdout="completed\n"))
        )
        context = orca_external.AgentContext()

        asyncio.run(
            orca_external.OrcaExternalAgent().run(
                "finish the task", environment, context
            )
        )

        self.assertFalse(hasattr(context, "output"))
        environment.exec.assert_awaited_once()

    def test_readme_uses_supported_harbor_filters(self) -> None:
        readme = (Path(__file__).parent / "README.md").read_text(encoding="utf-8")

        self.assertNotIn("--filter-difficulty", readme)
        self.assertIn("--include-task-name", readme)


if __name__ == "__main__":
    unittest.main()
