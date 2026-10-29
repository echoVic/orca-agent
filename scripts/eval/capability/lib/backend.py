"""Execution backends: run one scenario attempt and collect evidence.

`local` runs the agent on the host against a copied fixture (fast; used for the synthetic axes).
The backend never trusts the agent's claim of success: `outcome` comes from the verifier, and the
failures the runtime emits are mapped to `triage` signals.
"""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path

from . import scenario as scenario_lib
from . import triage

REPO = Path(__file__).resolve().parents[4]
DEFAULT_BINARY = REPO / "target" / "release" / "orca"
MOCK_PROVIDER = REPO / "scripts" / "eval" / "mock_provider.py"
PROTECTED_DIRS = ("tests",)


class BackendError(RuntimeError):
    pass


@dataclass
class Attempt:
    outcome: str
    failure_class: str | None
    wall_seconds: float
    verify_seconds: float
    agent_exit_code: int | None = None
    agent_terminal: str | None = None
    tool_calls: int | None = None
    tokens_out: int | None = None
    tokens_in: int | None = None
    cost_usd: float | None = None
    false_completion: bool = False
    harness_error: str | None = None
    environment_error: str | None = None
    product_signals: list[str] = field(default_factory=list)
    verifier_partial: bool = False
    cross_budget_passed: bool = False
    artifacts: dict = field(default_factory=dict)

    def as_dict(self) -> dict:
        return dict(self.__dict__)


def _free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


class MockProvider:
    """The scripted provider, scoped to one attempt."""

    def __init__(self, log_path: Path) -> None:
        self.port = _free_port()
        self.process = subprocess.Popen(
            [sys.executable, str(MOCK_PROVIDER), "--port", str(self.port), "--log", str(log_path)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            try:
                with socket.create_connection(("127.0.0.1", self.port), timeout=0.2):
                    return
            except OSError:
                time.sleep(0.1)
        self.stop()
        raise BackendError("mock provider did not start")

    def stop(self) -> None:
        if self.process.poll() is None:
            self.process.kill()


def agent_env(home: Path, provider: str, mock: MockProvider | None, extra_path: Path | None = None) -> dict[str, str]:
    env = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith(("ORCA_", "DEEPSEEK_", "OPENAI_", "ANTHROPIC_")) and key != "NODE_OPTIONS"
    }
    env.update(ORCA_HOME=str(home), TERM="xterm-256color")
    if extra_path is not None and extra_path.is_dir():
        env["PATH"] = f"{extra_path}:{env.get('PATH', '')}"
    if provider == "mock":
        env.update(ORCA_API_KEY="mock", DEEPSEEK_API_KEY="mock")
        if mock:
            env["ORCA_BASE_URL"] = f"http://127.0.0.1:{mock.port}"
    else:
        auth = Path.home() / ".orca" / "auth.json"
        try:
            key = json.loads(auth.read_text(encoding="utf-8"))["DEEPSEEK_API_KEY"]
        except (OSError, ValueError, KeyError) as error:
            raise BackendError(f"cannot read DEEPSEEK_API_KEY from {auth}: {error}") from None
        env["DEEPSEEK_API_KEY"] = key
    return env


def hash_protected(root: Path) -> dict[str, str]:
    digests: dict[str, str] = {}
    for name in PROTECTED_DIRS:
        target = root / name
        if not target.exists():
            continue
        for path in sorted(target.rglob("*")):
            if path.is_file():
                digests[str(path.relative_to(root))] = hashlib.sha256(path.read_bytes()).hexdigest()
    return digests


def parse_events(path: Path) -> dict:
    """Read `orca exec --output-format jsonl` output: tool calls, output tokens, terminal status."""
    tool_calls = 0
    tokens_out = None
    tokens_in = None
    cost_usd = None
    terminal = None
    if not path.exists():
        return {"tool_calls": None, "tokens_out": None, "tokens_in": None, "cost_usd": None, "terminal": None}
    for line in path.read_text(encoding="utf-8", errors="replace").splitlines():
        if '"tool.call.requested"' in line:
            tool_calls += 1
            continue
        if '"usage.updated"' in line:
            # Cumulative per run: the last event wins.
            try:
                payload = json.loads(line).get("payload") or {}
            except json.JSONDecodeError:
                continue
            tokens_in = payload.get("input_tokens", tokens_in)
            tokens_out = payload.get("output_tokens", tokens_out)
            cost_usd = payload.get("estimated_cost_usd", cost_usd)
            continue
        if '"session.completed"' not in line and '"usage"' not in line:
            continue
        try:
            record = json.loads(line)
        except json.JSONDecodeError:
            continue
        payload = record.get("payload") or {}
        terminal = terminal or payload.get("status")
        usage = payload.get("usage") or {}
        if usage.get("output_tokens") is not None:
            tokens_out = usage["output_tokens"]
        if usage.get("input_tokens") is not None:
            tokens_in = usage["input_tokens"]
        if payload.get("output_tokens") is not None:
            tokens_out = payload["output_tokens"]
        if payload.get("input_tokens") is not None:
            tokens_in = payload["input_tokens"]
    return {"tool_calls": tool_calls, "tokens_out": tokens_out, "tokens_in": tokens_in,
            "cost_usd": cost_usd, "terminal": terminal}


def run_local(
    scenario: scenario_lib.Scenario,
    attempt_dir: Path,
    binary: Path = DEFAULT_BINARY,
    provider: str | None = None,
) -> Attempt:
    """One attempt of a `local` scenario; artifacts go under `attempt_dir`."""
    attempt_dir.mkdir(parents=True, exist_ok=True)
    provider = provider or scenario.provider
    started = time.monotonic()
    workspace = attempt_dir / "workspace"
    home = attempt_dir / "home"
    home.mkdir(parents=True, exist_ok=True)
    shutil.copytree(scenario.fixture, workspace)
    (home / "config.toml").write_text("auto_memory = false\n", encoding="utf-8")
    if not (workspace / ".git").exists():
        subprocess.run(["git", "init", "-q", str(workspace)], capture_output=True, timeout=30)
    if scenario.prepare is not None:
        prepare_process_env = {**os.environ, **scenario.prepare_env}
        prepared = subprocess.run(
            [sys.executable, str(scenario.prepare)], cwd=workspace, capture_output=True, text=True,
            timeout=120, env=prepare_process_env,
        )
        (attempt_dir / "prepare.log").write_text(prepared.stdout + prepared.stderr, encoding="utf-8")
        if prepared.returncode != 0:
            return Attempt(
                outcome="error", failure_class="environment", wall_seconds=0.0, verify_seconds=0.0,
                environment_error=f"prepare script failed: {prepared.stderr.strip()[:300]}",
                artifacts={"workspace": str(workspace)},
            )
    try:
        subprocess.run(
            [str(binary), "trust", "add", "--cwd", str(workspace)],
            env=agent_env(home, provider, None),
            cwd=workspace,
            capture_output=True,
            timeout=30,
        )
    except (OSError, subprocess.SubprocessError) as error:
        return Attempt(
            outcome="error", failure_class="environment", wall_seconds=0.0, verify_seconds=0.0,
            environment_error=f"cannot trust workspace: {error}",
            artifacts={"workspace": str(workspace)},
        )

    if scenario.command == "goal":
        from . import goal_episode  # noqa: PLC0415

        goal_started = time.monotonic()
        episode = goal_episode.run(scenario, attempt_dir, binary)
        goal_wall = time.monotonic() - goal_started
        verify_env = {
            **os.environ,
            "CAPABILITY_ORCA_BIN": str(binary),
            "CAPABILITY_GOAL_OUTCOME": str(attempt_dir / "goal-outcome.json"),
            **scenario.verify_env,
        }
        verifier_results = []
        verify_started = time.monotonic()
        for index, command in enumerate(scenario.verify):
            try:
                result = subprocess.run(
                    ["bash", "-lc", command], cwd=workspace, capture_output=True, text=True,
                    timeout=scenario.limits.get("verify_seconds", 120), env=verify_env,
                )
                code, output = result.returncode, result.stdout + result.stderr
            except subprocess.TimeoutExpired:
                code, output = 124, f"verifier timed out: {command}\n"
            (attempt_dir / f"verify-{index}.log").write_text(output, encoding="utf-8")
            verifier_results.append({"command": command, "exit": code})
        goal_verify = time.monotonic() - verify_started
        failures = [item for item in verifier_results if item["exit"] != 0]
        harness_error = None
        if episode.get("error"):
            harness_error = episode["error"]
            outcome = "error"
            failure_class = "harness"
        elif failures:
            outcome, failure_class = "fail", "capability_gap"
        else:
            outcome, failure_class = "pass", None
        (attempt_dir / "attempt.json").write_text(
            json.dumps(
                {"outcome": outcome, "failure_class": failure_class, "episode": episode,
                 "verifier": verifier_results, "wall_seconds": round(goal_wall, 2),
                 "verify_seconds": round(goal_verify, 2)},
                indent=2,
            ) + "\n",
            encoding="utf-8",
        )
        return Attempt(
            outcome=outcome,
            failure_class=failure_class,
            wall_seconds=goal_wall,
            verify_seconds=goal_verify,
            agent_exit_code=0,
            harness_error=harness_error,
            artifacts={"goal_outcome": str(attempt_dir / "goal-outcome.json")},
        )

    protected = hash_protected(workspace)
    mock = None
    stdout_path = attempt_dir / "agent.stdout.jsonl"
    stderr_path = attempt_dir / "agent.stderr.txt"
    agent_exit: int | None = None
    wall_limit = scenario.limits.get("wall_seconds", 1800)
    if scenario.command == "workflow":
        stdout_path = attempt_dir / "workflow.stdout.txt"
        stderr_path = attempt_dir / "workflow.stderr.txt"
    try:
        if provider == "mock":
            mock = MockProvider(attempt_dir / "provider-requests.jsonl")
        argv = (
            [str(binary), "exec", "--mode", "full-auto", "--output-format", "jsonl",
             "--cwd", str(workspace), "--", scenario.prompt]
            if scenario.command == "exec"
            else [str(binary), "workflow", "run", str(workspace / scenario.script), "--cwd", str(workspace)]
        )
        with stdout_path.open("w", encoding="utf-8") as stdout, stderr_path.open("w", encoding="utf-8") as stderr:
            process = subprocess.run(
                argv,
                env=agent_env(home, provider, mock, extra_path=workspace / ".faultbin"),
                cwd=workspace,
                stdout=stdout,
                stderr=stderr,
                timeout=wall_limit,
            )
        agent_exit = process.returncode
        if scenario.command == "workflow":
            deadline = time.monotonic() + wall_limit
            final_state = None
            while time.monotonic() < deadline:
                listing = subprocess.run(
                    [str(binary), "workflow", "list"], cwd=workspace, capture_output=True, text=True,
                    timeout=30, env=agent_env(home, provider, None),
                )
                try:
                    runs = json.loads(listing.stdout or "[]")
                except json.JSONDecodeError:
                    runs = []
                local = [run for run in runs if run.get("cwd") == str(workspace)]
                if local and local[-1].get("status") in ("completed", "failed", "stopped"):
                    final_state = local[-1]
                    break
                time.sleep(1)
            (attempt_dir / "workflow-final.json").write_text(
                json.dumps(final_state, indent=2) + "\n", encoding="utf-8"
            )
            if final_state is not None:
                agent_exit = 0 if final_state.get("status") == "completed" else 1
    except subprocess.TimeoutExpired:
        agent_exit = None
    finally:
        if mock:
            mock.stop()
    wall = time.monotonic() - started
    events = parse_events(stdout_path) if scenario.command == "exec" else {"tool_calls": None, "tokens_out": None,
                                                                          "tokens_in": None, "cost_usd": None,
                                                                          "terminal": None}

    verify_started = time.monotonic()
    verifier_results: list[dict] = []
    for index, command in enumerate(scenario.verify):
        verify_process_env = {**os.environ, "CAPABILITY_ORCA_BIN": str(binary), **scenario.verify_env}
        try:
            result = subprocess.run(
                ["bash", "-lc", command], cwd=workspace, capture_output=True, text=True,
                timeout=scenario.limits.get("verify_seconds", 600), env=verify_process_env,
            )
            code = result.returncode
            output = result.stdout + result.stderr
        except subprocess.TimeoutExpired:
            code = 124
            output = f"verifier timed out: {command}\n"
        except OSError as error:
            code = 127
            output = f"verifier could not start: {error}\n"
        (attempt_dir / f"verify-{index}.log").write_text(output, encoding="utf-8")
        verifier_results.append({"command": command, "exit": code})
    verify_seconds = time.monotonic() - verify_started

    after = hash_protected(workspace)
    tampered = sorted(path for path, digest in protected.items() if after.get(path) != digest)
    failures = [item for item in verifier_results if item["exit"] != 0]
    verifier_partial = 0 < len(failures) < len(verifier_results)

    signals = []
    if agent_exit == 137:
        signals.append("exit_137")
    if agent_exit is not None and events.get("tool_calls") == 0 and not failures:
        signals.append("no_tool_calls")

    outcome = "pass" if not failures and not tampered else "fail"
    evidence = triage.Evidence(
        outcome=outcome,
        wall_seconds=wall,
        wall_limit=wall_limit,
        agent_exit_code=agent_exit,
        agent_terminal=events.get("terminal"),
        tool_calls=events.get("tool_calls"),
        false_completion=(agent_exit == 0 and bool(failures)),
        product_signals=signals,
        verifier_partial=verifier_partial and (agent_exit == 0),
    )
    failure_class = None if outcome == "pass" else triage.classify(evidence)
    if tampered:
        outcome = "error"
        failure_class = "harness"
    (attempt_dir / "attempt.json").write_text(
        json.dumps(
            {
                "outcome": outcome,
                "failure_class": failure_class,
                "agent_exit": agent_exit,
                "tampered": tampered,
                "verifier": verifier_results,
                "events": events,
                "wall_seconds": round(wall, 2),
                "verify_seconds": round(verify_seconds, 2),
            },
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )
    return Attempt(
        outcome=outcome,
        failure_class=failure_class,
        wall_seconds=wall,
        verify_seconds=verify_seconds,
        agent_exit_code=agent_exit,
        agent_terminal=events.get("terminal"),
        tool_calls=events.get("tool_calls"),
        tokens_out=events.get("tokens_out"),
        tokens_in=events.get("tokens_in"),
        cost_usd=events.get("cost_usd"),
        false_completion=(agent_exit == 0 and bool(failures)),
        harness_error=f"protected files were modified: {tampered}" if tampered else None,
        verifier_partial=verifier_partial and (agent_exit == 0),
        artifacts={"workspace": str(workspace), "agent_stdout": str(stdout_path)},
    )


def run_docker(scenario, attempt_dir: Path, binary: Path = DEFAULT_BINARY, provider: str | None = None) -> Attempt:
    """Run one attempt inside a Linux container (the design's `docker` backend for B/D)."""
    attempt_dir.mkdir(parents=True, exist_ok=True)
    provider = provider or scenario.provider
    if scenario.provider == "mock" or provider == "mock":
        raise BackendError("the docker backend needs a real provider (no host-network mock wiring yet)")
    if not scenario.image:
        raise BackendError("docker scenarios need an `image`")
    if provider != "deepseek":
        raise BackendError(f"unsupported docker provider: {provider}")
    musl = REPO / "target" / "x86_64-unknown-linux-musl" / "release" / "orca"
    if not musl.exists():
        return Attempt(outcome="error", failure_class="environment", wall_seconds=0.0, verify_seconds=0.0,
                       environment_error=f"musl binary missing: {musl}")
    try:
        key = json.loads((Path.home() / ".orca" / "auth.json").read_text(encoding="utf-8"))["DEEPSEEK_API_KEY"]
    except (OSError, ValueError, KeyError) as error:
        return Attempt(outcome="error", failure_class="environment", wall_seconds=0.0, verify_seconds=0.0,
                       environment_error=f"cannot read API key: {error}")
    started = time.monotonic()
    workspace = attempt_dir / "workspace"
    home = attempt_dir / "home"
    home.mkdir(parents=True, exist_ok=True)
    (home / "config.toml").write_text("auto_memory = false\n", encoding="utf-8")
    shutil.copytree(scenario.fixture, workspace)
    if scenario.prepare is not None:
        prepared = subprocess.run([sys.executable, str(scenario.prepare)], cwd=workspace, capture_output=True,
                                  text=True, timeout=120, env={**os.environ, **scenario.prepare_env})
        if prepared.returncode != 0:
            return Attempt(outcome="error", failure_class="environment", wall_seconds=0.0, verify_seconds=0.0,
                           environment_error=f"prepare failed: {prepared.stderr.strip()[:200]}")
    (attempt_dir / "prompt.txt").write_text(scenario.prompt, encoding="utf-8")
    protected = hash_protected(workspace)
    mounts = [
        "-v", f"{workspace}:/workspace:{'ro' if scenario.workspace_mode == 'ro' else 'rw'}",
        "-v", f"{musl.parent}:/mnt/orca-bin:ro",
        "-v", f"{home}:/home/orca", "-v", f"{attempt_dir}:/artifacts",
    ]
    for extra in scenario.docker_args:
        mounts.append(str(extra).replace("{workspace}", str(workspace)))
    agent_command = (
        "/mnt/orca-bin/orca trust add --cwd /workspace >/dev/null 2>&1 || true; "
        "exec /mnt/orca-bin/orca exec --mode full-auto --output-format jsonl --cwd /workspace "
        "--api-key \"$DEEPSEEK_API_KEY\" -- \"$(cat /artifacts/prompt.txt)\""
    )
    agent_exit: int | None = None
    stdout_path = attempt_dir / "agent.stdout.jsonl"
    stderr_path = attempt_dir / "agent.stderr.txt"
    try:
        with stdout_path.open("w", encoding="utf-8") as stdout, stderr_path.open("w", encoding="utf-8") as stderr:
            process = subprocess.run(
                ["docker", "run", "--rm", *mounts, "-e", "ORCA_HOME=/home/orca",
                 "-e", f"DEEPSEEK_API_KEY={key}", "-w", "/workspace", scenario.image,
                 "bash", "-lc", agent_command],
                stdout=stdout, stderr=stderr, timeout=scenario.limits.get("wall_seconds", 1800),
            )
        agent_exit = process.returncode
    except subprocess.TimeoutExpired:
        agent_exit = None
    wall = time.monotonic() - started
    events = parse_events(stdout_path)
    verify_started = time.monotonic()
    verify_command = " && ".join(scenario.verify)
    verify_env = {
        **scenario.verify_env,
        "CAPABILITY_AGENT_EXIT": "" if agent_exit is None else str(agent_exit),
        "CAPABILITY_AGENT_TERMINAL": str(events.get("terminal") or ""),
    }
    verify_env_args = []
    for key, value in verify_env.items():
        verify_env_args += ["-e", f"{key}={value}"]
    verify_process = subprocess.run(
        ["docker", "run", "--rm", *mounts, *verify_env_args, "-w", "/workspace", scenario.image,
         "bash", "-lc", verify_command],
        capture_output=True, text=True, timeout=scenario.limits.get("verify_seconds", 600),
    )
    (attempt_dir / "verify.log").write_text(verify_process.stdout + verify_process.stderr, encoding="utf-8")
    verify_seconds = time.monotonic() - verify_started
    after = hash_protected(workspace)
    tampered = sorted(path for path, digest in protected.items() if after.get(path) != digest)
    outcome = "pass" if verify_process.returncode == 0 and not tampered else "fail"
    evidence = triage.Evidence(outcome=outcome, wall_seconds=wall, wall_limit=scenario.limits.get("wall_seconds", 1800),
                               agent_exit_code=agent_exit, tool_calls=events.get("tool_calls"),
                               false_completion=(agent_exit == 0 and verify_process.returncode != 0),
                               product_signals=(["exit_137"] if agent_exit == 137 else []))
    failure_class = None if outcome == "pass" else triage.classify(evidence)
    if tampered:
        outcome, failure_class = "error", "harness"
    (attempt_dir / "attempt.json").write_text(json.dumps({
        "outcome": outcome, "failure_class": failure_class, "agent_exit": agent_exit, "tampered": tampered,
        "verifier": [{"command": verify_command, "exit": verify_process.returncode}], "events": events,
        "wall_seconds": round(wall, 2), "verify_seconds": round(verify_seconds, 2)}, indent=2) + "\n",
        encoding="utf-8")
    return Attempt(outcome=outcome, failure_class=failure_class, wall_seconds=wall, verify_seconds=verify_seconds,
                   agent_exit_code=agent_exit, tool_calls=events.get("tool_calls"), tokens_out=events.get("tokens_out"),
                   tokens_in=events.get("tokens_in"), cost_usd=events.get("cost_usd"),
                   false_completion=(agent_exit == 0 and verify_process.returncode != 0),
                   harness_error=f"protected files were modified: {tampered}" if tampered else None,
                   artifacts={"workspace": str(workspace), "agent_stdout": str(stdout_path)})
