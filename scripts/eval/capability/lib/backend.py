"""Execution backends: run one scenario attempt and collect evidence.

`local` runs the agent on the host against a copied fixture (fast; used for the synthetic axes).
The backend never trusts the agent's claim of success: `outcome` comes from the verifier, and the
failures the runtime emits are mapped to `triage` signals.
"""

from __future__ import annotations

import hashlib
import json
import os
import posixpath
import math
import re
import shlex
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path

from . import scenario as scenario_lib
from . import triage, evidence as evidence_lib

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
    protocol: dict = field(default_factory=dict)
    discoveries: list[dict] = field(default_factory=list)
    compactions: list[dict] = field(default_factory=list)
    provider_observations: list[dict] = field(default_factory=list)
    final_verification: str = "unknown"

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
    env.update(ORCA_HOME=str(home), TERM="xterm-256color", PYTHONDONTWRITEBYTECODE="1")
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
        if not target.exists() and not target.is_symlink():
            continue
        for path in [target, *sorted(target.rglob("*"))]:
            digests[str(path.relative_to(root))] = evidence_lib.file_identity(path)
    return digests


def event_observation(value, required: tuple[str, ...], numeric: tuple[str, ...]) -> dict:
    """Retain additive measurement fields; unavailable values stay unknown, including nulls."""
    def known(item):
        if item is None:
            return "unknown"
        if isinstance(item, dict):
            return {key: known(value) for key, value in item.items()}
        if isinstance(item, list):
            return [known(value) for value in item]
        if isinstance(item, float) and not math.isfinite(item):
            return "unknown"
        return item
    observation = known(value) if isinstance(value, dict) else {}
    for key in required:
        observation.setdefault(key, "unknown")
    for key in numeric:
        item = observation.get(key)
        if isinstance(item, bool) or not isinstance(item, (int, float)) or not math.isfinite(item) or item < 0:
            observation[key] = "unknown"
    if "first_delta_kind" in required and (not isinstance(observation["first_delta_kind"], str) or not observation["first_delta_kind"]):
        observation["first_delta_kind"] = "unknown"
    return observation


def parse_events(path: Path, constraints: list[dict] | None = None, workspace: Path | str | None = None) -> dict:
    """Parse real JSONL records. Discovery requires a successful paired tool result."""
    result = {"tool_calls": None, "tokens_out": None, "tokens_in": None, "cost_usd": None,
              "terminal": None, "discoveries": [], "compactions": [], "provider_observations": []}
    if not path.exists():
        return result
    result["tool_calls"] = 0
    calls, found = {}, set()
    for index, line in enumerate(path.read_text(errors="replace").splitlines(), 1):
        try:
            record = json.loads(line)
        except ValueError:
            continue
        if not isinstance(record, dict): continue
        kind = record.get("type") or record.get("event")
        payload = record.get("payload") or {}
        if not isinstance(payload, dict): continue
        if kind == "tool.call.requested":
            result["tool_calls"] += 1
            if isinstance(payload.get("id"), str):
                calls[payload["id"]] = (payload, result["tool_calls"])
        elif kind == "tool.call.completed":
            call_id = payload.get("id")
            pair = calls.get(call_id) if isinstance(call_id, str) else None
            if (not pair or payload.get("status") != "completed" or payload.get("error")
                    or payload.get("exit_code") not in (None, 0)):
                continue
            request, count = pair
            if request.get("name") not in {"read_file", "grep", "file_search"}:
                continue
            targets = [request.get("target")]
            arguments = request.get("raw_arguments")
            if isinstance(arguments, str):
                try:
                    arguments = json.loads(arguments)
                except ValueError:
                    arguments = None
            if isinstance(arguments, dict):
                targets.extend(arguments.get(key) for key in ("path", "file_path", "target"))
            def normalized(value):
                if not isinstance(value, str) or not value:
                    return None
                if value.startswith("/"):
                    if workspace is None:
                        return None
                    base = posixpath.normpath(str(workspace))
                    absolute = posixpath.normpath(value)
                    if not absolute.startswith(base.rstrip("/") + "/"):
                        return None
                    return posixpath.relpath(absolute, base)
                value = posixpath.normpath(value)
                return None if value == ".." or value.startswith("../") else value
            targets = {normalized(value) for value in targets} - {None}
            output = payload.get("output")
            if not isinstance(output, str): output = json.dumps(output)
            for constraint in constraints or []:
                if constraint["id"] in found: continue
                source = constraint["source"]
                source_pattern = r"(?<![\w./-])" + re.escape(source) + r"(?![\w./-])"
                exact_target = posixpath.normpath(source) in targets
                attributed_line = any(re.search(source_pattern, line) and constraint["contains"] in line
                                      for line in output.splitlines())
                if not ((exact_target and constraint["contains"] in output) or attributed_line):
                    continue
                found.add(constraint["id"])
                result["discoveries"].append({"constraint_id": constraint["id"], "source": source,
                    "tool_call_id": payload["id"], "tool_name": request.get("name"), "event_line": index,
                    "tool_calls_to_discovery": count,
                    "tokens_in_to_discovery": result["tokens_in"] if result["tokens_in"] is not None else "unknown",
                    "tokens_out_to_discovery": result["tokens_out"] if result["tokens_out"] is not None else "unknown",
                    "token_basis": "last observed cumulative usage before discovery; not exact attribution",
                    "output_sha256": hashlib.sha256(output.encode()).hexdigest()})
        elif kind in {"usage.updated", "session.completed"}:
            usage = payload.get("usage") or payload
            if not isinstance(usage, dict):
                usage = {}
            for key in ("tokens_in", "tokens_out", "cost_usd"):
                source = {"tokens_in": "input_tokens", "tokens_out": "output_tokens", "cost_usd": "estimated_cost_usd"}[key]
                value = usage.get(source)
                if not isinstance(value, bool) and isinstance(value, (int, float)) and math.isfinite(value) and value >= 0:
                    result[key] = value
            if kind == "session.completed": result["terminal"] = payload.get("status")
        elif kind == "context.compacted":
            observation = event_observation(payload.get("observation"), (
                "before_tokens_est", "after_tokens_est", "elapsed_ms", "summary_requests", "summary_usage",
                "summary_usage_complete", "estimated_cost_usd", "cache_hit_ratio", "prefix_reuse_estimate"),
                ("before_tokens_est", "after_tokens_est", "elapsed_ms", "estimated_cost_usd", "cache_hit_ratio"))
            result["compactions"].append({"event_line": index, "type": kind,
                **{key: payload.get(key) if payload.get(key) is not None else "unknown" for key in (
                    "reason", "strategy", "before_messages", "after_messages", "collapsed_messages")},
                "observation": observation})
        elif kind in {"provider.response.completed", "model.response.completed"}:
            result["provider_observations"].append({"event_line": index, "type": kind,
                "observation": event_observation(payload.get("observation"),
                    ("elapsed_ms", "first_delta_ms", "first_delta_kind"), ("elapsed_ms", "first_delta_ms"))})
        elif isinstance(kind, str) and kind.startswith("context.compaction."):
            result["compactions"].append({"event_line": index, "type": kind})
    return result


def protected_evidence(item, workspace: Path) -> dict[str, str]:
    hashes = hash_protected(workspace)
    paths = evidence_lib.verifier_paths(item, workspace) + evidence_lib.verifier_paths(item, item.fixture)
    paths += [p for p in (item.path, item.prepare) if p is not None]
    # Include types, permissions, and additions under declared readonly directories.
    for base in (workspace, item.fixture):
        for relative in item.readonly_inputs:
            path = base / relative
            paths.append(path)
            if path.is_dir():
                paths.extend(path.rglob("*"))
    if item.verifier_root is not None:
        paths += [item.verifier_root, *item.verifier_root.rglob("*")]
    for path in paths:
        hashes[str(path.absolute())] = evidence_lib.file_identity(path)
    return hashes


def changed_protected(before: dict, after: dict) -> list[str]:
    return sorted(path for path in before.keys() | after.keys() if before.get(path) != after.get(path))


def snapshot_evidence(directory: Path) -> dict[str, str]:
    return {str(path.absolute()): evidence_lib.file_identity(path)
            for path in [directory, *sorted(directory.rglob("*"))]}


def protected_state(item, workspace: Path, attempt_dir: Path) -> dict:
    return {**protected_evidence(item, workspace), **snapshot_evidence(attempt_dir / "verifier")}


def snapshot_verifiers(item, workspace: Path, attempt_dir: Path, container: bool = False) -> tuple[list[str], dict]:
    """Freeze programs and explicitly readonly inputs; keep outputs in the live cwd.

    The agent container never mounts these copies. Local chmod plus pre/post guards
    provide evidence integrity, not a security boundary against hostile host code.
    """
    directory = attempt_dir / "verifier"
    directory.mkdir(exist_ok=True)
    if item.verifier_root is not None:
        shutil.copytree(item.verifier_root, directory / "external", symlinks=False)
    replacements = {}
    for token, original, relative in evidence_lib.verifier_bindings(item, workspace):
        if Path(relative).is_absolute() or ".." in Path(relative).parts:
            raise BackendError(f"verifier files must use relative paths: {token}")
        if not original.is_file() and not original.is_dir():
            raise BackendError(f"verifier input missing after prepare: {token}")
        snapshot = directory / relative
        if not token.startswith("@verifier/"):
            snapshot.parent.mkdir(parents=True, exist_ok=True)
            if original.is_dir():
                shutil.copytree(original, snapshot, dirs_exist_ok=True, symlinks=False)
            else:
                shutil.copy2(original, snapshot)
        replacements[token] = str(Path("/verifier") / relative) if container else str(snapshot)
    # Preserve execute/read bits while removing write permissions. Directory mode
    # must retain traversal. Bytecode writes are disabled in verifier environments.
    for path in [*directory.rglob("*"), directory]:
        path.chmod(path.stat().st_mode & ~0o222)
    commands = []
    for command in item.verify:
        # Replace only whole shell words, preserving operators and original quoting.
        # Substring substitutions could corrupt output filenames or quoted code.
        def rewrite(match):
            word = match.group(0)
            try:
                tokens = shlex.split(word)
            except ValueError:
                return word
            return shlex.quote(replacements[tokens[0]]) if len(tokens) == 1 and tokens[0] in replacements else word
        commands.append(re.sub(r"(?:[^\s'\";&|<>]|'[^']*'|\"[^\"]*\")+", rewrite, command))
    return commands, snapshot_evidence(directory)


def run_local(
    scenario: scenario_lib.Scenario,
    attempt_dir: Path,
    binary: Path = DEFAULT_BINARY,
    provider: str | None = None,
    execution: dict | None = None,
) -> Attempt:
    """One attempt of a `local` scenario; artifacts go under `attempt_dir`."""
    attempt_dir.mkdir(parents=True, exist_ok=True)
    provider = provider or scenario.provider
    config = dict(scenario.execution)
    config.update(execution or {})
    evidence_lib.validate_execution(config)
    if scenario.command == "goal" and config:
        raise BackendError("goal episode does not support explicit execution overrides")
    if scenario.command == "workflow" and set(config).intersection(evidence_lib.BUDGET_KEYS):
        raise BackendError("workflow does not support exec budget overrides")
    attempt_protocol = evidence_lib.execution_protocol(config)
    started = time.monotonic()
    workspace = attempt_dir / "workspace"
    home = attempt_dir / "home"
    home.mkdir(parents=True, exist_ok=True)
    shutil.copytree(scenario.fixture, workspace)
    (home / "config.toml").write_text("auto_memory = false\n", encoding="utf-8")
    if not (workspace / ".git").exists():
        subprocess.run(["git", "init", "-q", str(workspace)], capture_output=True, timeout=30)
    if scenario.prepare is not None:
        prepare_process_env = {**os.environ, "CAPABILITY_EXTERNAL_DB": str(attempt_dir / "external-state.sqlite"), **scenario.prepare_env}
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

    protected = protected_evidence(scenario, workspace)
    verifier_commands, snapshot_hashes = snapshot_verifiers(scenario, workspace, attempt_dir)
    protected.update(snapshot_hashes)
    attempt_protocol.update(evidence_lib.scenario_identity(scenario, workspace))
    attempt_protocol.update(binary_sha256=evidence_lib.file_sha(binary), provider=provider, backend="local")

    if scenario.command == "goal":
        from . import goal_episode  # noqa: PLC0415

        goal_started = time.monotonic()
        episode = goal_episode.run(scenario, attempt_dir, binary)
        goal_wall = time.monotonic() - goal_started
        verify_env = {
            **os.environ,
            "CAPABILITY_ORCA_BIN": str(binary),
            "CAPABILITY_GOAL_OUTCOME": str(attempt_dir / "goal-outcome.json"),
            "PYTHONDONTWRITEBYTECODE": "1",
            **scenario.verify_env,
        }
        verifier_results = []
        verifier_outputs: list[str] = []
        verify_started = time.monotonic()
        tampered = changed_protected(protected, protected_state(scenario, workspace, attempt_dir))
        for index, command in enumerate(verifier_commands):
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
            verifier_outputs.append(output)
        goal_verify = time.monotonic() - verify_started
        failures = [item for item in verifier_results if item["exit"] != 0]
        tampered = sorted(set(tampered) | set(changed_protected(protected, protected_state(scenario, workspace, attempt_dir))))
        final_verification = "failed" if failures or tampered else "passed"
        harness_error = None
        if tampered:
            harness_error = f"protected files were modified: {tampered}"
            outcome, failure_class = "error", "harness"
        elif episode.get("error"):
            harness_error = episode["error"]
            outcome = "error"
            failure_class = "harness"
        elif failures:
            evidence = triage.Evidence(outcome="fail", verifier_output="\n".join(verifier_outputs))
            outcome, failure_class = "fail", triage.classify(evidence)
        else:
            outcome, failure_class = "pass", None
        (attempt_dir / "attempt.json").write_text(
            json.dumps(
                {"outcome": outcome, "failure_class": failure_class, "episode": episode,
                 "verifier": verifier_results, "wall_seconds": round(goal_wall, 2),
                 "protocol": attempt_protocol, "final_verification": final_verification, "tampered": tampered,
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
            protocol=attempt_protocol, final_verification=final_verification,
            artifacts={"goal_outcome": str(attempt_dir / "goal-outcome.json")},
        )

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
             "--cwd", str(workspace), *evidence_lib.exec_options(config), "--", scenario.prompt]
            if scenario.command == "exec"
            else [str(binary), "workflow", "run", str(workspace / scenario.script), "--cwd", str(workspace)]
        )
        env = agent_env(home, provider, mock, extra_path=workspace / ".faultbin")
        env["CAPABILITY_EXTERNAL_DB"] = str(attempt_dir / "external-state.sqlite")
        if config.get("reasoning_effort") is not None: env["ORCA_REASONING_EFFORT"] = config["reasoning_effort"]
        if config.get("model") is not None: env["ORCA_MODEL"] = config["model"]
        if config.get("endpoint") is not None: env["ORCA_BASE_URL"] = config["endpoint"]
        # A mock run must always stay on the offline endpoint, even with an explicit URL override.
        if mock is not None:
            env["ORCA_BASE_URL"] = f"http://127.0.0.1:{mock.port}"
            # Fingerprint the actual scripted fixture and its response-affecting
            # watch configuration; the listening port is provenance, not behavior.
            script_sha = evidence_lib.file_sha(MOCK_PROVIDER)
            fixture_sha = evidence_lib.digest({"script_sha256": script_sha,
                "mock_watch": env.get("MOCK_WATCH", "")}) if script_sha != "unknown" else "unknown"
            attempt_protocol["provider_fixture_sha256"] = fixture_sha
            if fixture_sha != "unknown":
                attempt_protocol["endpoint_comparison_identity"] = "controlled-mock:" + fixture_sha
        attempt_protocol["endpoint_identity"] = evidence_lib.endpoint_identity(env.get("ORCA_BASE_URL"))
        with stdout_path.open("w", encoding="utf-8") as stdout, stderr_path.open("w", encoding="utf-8") as stderr:
            process = subprocess.run(
                argv,
                env=env,
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
    events = parse_events(stdout_path, scenario.evidence, workspace=workspace) if scenario.command == "exec" else {"tool_calls": None, "tokens_out": None,
                                                                          "tokens_in": None, "cost_usd": None,
                                                                          "terminal": None}

    attempt_protocol = evidence_lib.attach_provider(attempt_protocol, attempt_dir / "provider-requests.jsonl")
    tampered = changed_protected(protected, protected_state(scenario, workspace, attempt_dir))
    verify_started = time.monotonic()
    verifier_results: list[dict] = []
    verifier_outputs: list[str] = []
    for index, command in enumerate(verifier_commands):
        verify_process_env = {**os.environ, "PYTHONDONTWRITEBYTECODE": "1", "CAPABILITY_ORCA_BIN": str(binary), "CAPABILITY_EXTERNAL_DB": str(attempt_dir / "external-state.sqlite"), **scenario.verify_env}
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
        verifier_outputs.append(output)
    verify_seconds = time.monotonic() - verify_started

    after = protected_state(scenario, workspace, attempt_dir)
    tampered = sorted(set(tampered) | set(changed_protected(protected, after)))
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
        verifier_output="\n".join(verifier_outputs),
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
                "compactions": events.get("compactions", []),
                "provider_observations": events.get("provider_observations", []),
                "protocol": attempt_protocol,
                "final_verification": "passed" if not failures and not tampered else "failed",
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
        protocol=attempt_protocol, discoveries=events.get("discoveries", []),
        compactions=events.get("compactions", []), provider_observations=events.get("provider_observations", []),
        final_verification="passed" if not failures and not tampered else "failed",
        artifacts={"workspace": str(workspace), "agent_stdout": str(stdout_path)},
    )


def run_docker(scenario, attempt_dir: Path, binary: Path = DEFAULT_BINARY, provider: str | None = None, execution: dict | None = None) -> Attempt:
    """Run one attempt inside a Linux container (the design's `docker` backend for B/D)."""
    attempt_dir.mkdir(parents=True, exist_ok=True)
    provider = provider or scenario.provider
    config = dict(scenario.execution); config.update(execution or {})
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
    verifier_commands, snapshot_hashes = snapshot_verifiers(scenario, workspace, attempt_dir, container=True)
    protected = protected_state(scenario, workspace, attempt_dir)
    attempt_protocol = evidence_lib.execution_protocol(config)
    attempt_protocol.update(evidence_lib.scenario_identity(scenario, workspace))
    attempt_protocol.update(binary_sha256=evidence_lib.file_sha(musl), provider=provider, backend="docker")
    mounts = [
        "-v", f"{workspace}:/workspace:{'ro' if scenario.workspace_mode == 'ro' else 'rw'}",
        "-v", f"{musl.parent}:/mnt/orca-bin:ro",
        "-v", f"{home}:/home/orca", "-v", f"{attempt_dir / 'prompt.txt'}:/prompt.txt:ro",
    ]
    for extra in scenario.docker_args:
        mounts.append(str(extra).replace("{workspace}", str(workspace)))
    agent_command = (
        "/mnt/orca-bin/orca trust add --cwd /workspace >/dev/null 2>&1 || true; "
        "exec /mnt/orca-bin/orca exec --mode full-auto --output-format jsonl --cwd /workspace "
        "--api-key \"$DEEPSEEK_API_KEY\" -- \"$(cat /prompt.txt)\""
    )
    if config:
        options = shlex.join(evidence_lib.exec_options(config))
        agent_command = agent_command.replace('--api-key', options + ' --api-key', 1)
    config_env = []
    for config_key, env_name in (("model", "ORCA_MODEL"), ("reasoning_effort", "ORCA_REASONING_EFFORT"), ("endpoint", "ORCA_BASE_URL")):
        if config.get(config_key) is not None:
            config_env += ["-e", f"{env_name}={config[config_key]}"]
    agent_exit: int | None = None
    stdout_path = attempt_dir / "agent.stdout.jsonl"
    stderr_path = attempt_dir / "agent.stderr.txt"
    try:
        with stdout_path.open("w", encoding="utf-8") as stdout, stderr_path.open("w", encoding="utf-8") as stderr:
            process = subprocess.run(
                ["docker", "run", "--rm", *mounts, "-e", "ORCA_HOME=/home/orca",
                 "-e", f"DEEPSEEK_API_KEY={key}", *config_env, "-w", "/workspace", scenario.image,
                 "bash", "-lc", agent_command],
                stdout=stdout, stderr=stderr, timeout=scenario.limits.get("wall_seconds", 1800),
            )
        agent_exit = process.returncode
    except subprocess.TimeoutExpired:
        agent_exit = None
    wall = time.monotonic() - started
    events = parse_events(stdout_path, scenario.evidence, workspace="/workspace")
    tampered = changed_protected(protected, protected_state(scenario, workspace, attempt_dir))
    verify_started = time.monotonic()
    verify_command = " && ".join(verifier_commands)
    verifier_mounts = [*mounts, "-v", f"{attempt_dir / 'verifier'}:/verifier:ro"]
    verify_env = {
        **scenario.verify_env,
        "PYTHONDONTWRITEBYTECODE": "1",
        "CAPABILITY_AGENT_EXIT": "" if agent_exit is None else str(agent_exit),
        "CAPABILITY_AGENT_TERMINAL": str(events.get("terminal") or ""),
    }
    verify_env_args = []
    for key, value in verify_env.items():
        verify_env_args += ["-e", f"{key}={value}"]
    verify_process = subprocess.run(
        ["docker", "run", "--rm", *verifier_mounts, *verify_env_args, "-w", "/workspace", scenario.image,
         "bash", "-lc", verify_command],
        capture_output=True, text=True, timeout=scenario.limits.get("verify_seconds", 600),
    )
    verifier_output = verify_process.stdout + verify_process.stderr
    (attempt_dir / "verify.log").write_text(verifier_output, encoding="utf-8")
    verify_seconds = time.monotonic() - verify_started
    after = protected_state(scenario, workspace, attempt_dir)
    tampered = sorted(set(tampered) | set(changed_protected(protected, after)))
    outcome = "pass" if verify_process.returncode == 0 and not tampered else "fail"
    evidence = triage.Evidence(outcome=outcome, wall_seconds=wall, wall_limit=scenario.limits.get("wall_seconds", 1800),
                               agent_exit_code=agent_exit, tool_calls=events.get("tool_calls"),
                               false_completion=(agent_exit == 0 and verify_process.returncode != 0),
                               product_signals=(["exit_137"] if agent_exit == 137 else []),
                               verifier_output=verifier_output)
    failure_class = None if outcome == "pass" else triage.classify(evidence)
    if tampered:
        outcome, failure_class = "error", "harness"
    (attempt_dir / "attempt.json").write_text(json.dumps({
        "outcome": outcome, "failure_class": failure_class, "agent_exit": agent_exit, "tampered": tampered,
        "verifier": [{"command": verify_command, "exit": verify_process.returncode}], "events": events,
        "protocol": attempt_protocol, "final_verification": "passed" if outcome == "pass" else "failed",
        "compactions": events.get("compactions", []), "provider_observations": events.get("provider_observations", []),
        "wall_seconds": round(wall, 2), "verify_seconds": round(verify_seconds, 2)}, indent=2) + "\n",
        encoding="utf-8")
    return Attempt(outcome=outcome, failure_class=failure_class, wall_seconds=wall, verify_seconds=verify_seconds,
                   agent_exit_code=agent_exit, tool_calls=events.get("tool_calls"), tokens_out=events.get("tokens_out"),
                   tokens_in=events.get("tokens_in"), cost_usd=events.get("cost_usd"),
                   false_completion=(agent_exit == 0 and verify_process.returncode != 0),
                   harness_error=f"protected files were modified: {tampered}" if tampered else None,
                   protocol=attempt_protocol, discoveries=events.get("discoveries", []),
                   compactions=events.get("compactions", []), provider_observations=events.get("provider_observations", []),
                   final_verification="passed" if outcome == "pass" else "failed",
                   artifacts={"workspace": str(workspace), "agent_stdout": str(stdout_path)})
