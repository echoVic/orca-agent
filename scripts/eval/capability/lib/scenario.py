"""Scenario loading and validation (schema v1).

A scenario is `scenario.json` + a fixture directory + an optional prompt file. The `verify` list is
run inside the workspace after the agent phase; `anti_cheat` names a check implemented in
`verify_helpers`. Everything is data so a reviewer can see the ladder rungs side by side.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from pathlib import Path

SCHEMA_VERSION = 1
REQUIRED = {
    "schema_version": int,
    "id": str,
    "axis": str,
    "rung": int,
    "difficulty": dict,
    "backend": str,
    "fixture": str,
    "verify": list,
}
BACKENDS = {"local", "harbor", "docker"}
AXES = {"A", "B", "C", "D", "E", "F", "G", "H", "S"}


class ScenarioError(ValueError):
    pass


@dataclass
class Scenario:
    id: str
    axis: str
    rung: int
    difficulty: dict
    backend: str
    fixture: Path
    verify: list[str]
    prompt: str = ""
    limits: dict = field(default_factory=dict)
    anti_cheat: list[str] = field(default_factory=list)
    diagnostics: list[str] = field(default_factory=list)
    provider: str = "mock"
    prepare: Path | None = None
    prepare_env: dict = field(default_factory=dict)
    verify_env: dict = field(default_factory=dict)
    image: str = ""
    docker_args: list = field(default_factory=list)
    workspace_mode: str = "rw"
    command: str = "exec"
    script: str = "workflow.js"
    path: Path | None = None
    execution: dict = field(default_factory=dict)
    evaluation: dict = field(default_factory=dict)
    evidence: list[dict] = field(default_factory=list)
    readonly_inputs: list[str] = field(default_factory=list)
    verifier_root: Path | None = None

    @property
    def difficulty_value(self) -> float:
        """A single monotone number for the ladder: `difficulty["level"]` or the primary knob."""
        for key in ("level", "files", "faults", "fanout", "turns", "steps"):
            value = self.difficulty.get(key)
            if isinstance(value, (int, float)):
                return float(value)
        return float(self.rung)


def load(path: Path) -> Scenario:
    if not path.is_file():
        raise ScenarioError(f"scenario file not found: {path}")
    raw = json.loads(path.read_text(encoding="utf-8"))
    for key, expected in REQUIRED.items():
        if key not in raw:
            raise ScenarioError(f"{path}: missing `{key}`")
        if not isinstance(raw[key], expected):
            raise ScenarioError(f"{path}: `{key}` must be {expected.__name__}")
    if raw["schema_version"] != SCHEMA_VERSION:
        raise ScenarioError(f"{path}: unsupported schema_version {raw['schema_version']}")
    if raw["axis"] not in AXES:
        raise ScenarioError(f"{path}: axis must be one of {sorted(AXES)}")
    if raw["backend"] not in BACKENDS:
        raise ScenarioError(f"{path}: backend must be one of {sorted(BACKENDS)}")
    root = path.parent
    fixture = (root / raw["fixture"]).resolve()
    if not fixture.is_dir():
        raise ScenarioError(f"{path}: fixture directory not found: {fixture}")
    prompt = ""
    if raw.get("prompt"):
        candidate = (root / raw["prompt"]).resolve()
        if candidate.is_file():
            prompt = candidate.read_text(encoding="utf-8").strip()
        elif raw.get("prompt_is_text"):
            prompt = str(raw["prompt"]).strip()
        else:
            raise ScenarioError(f"{path}: prompt file not found: {candidate}")
    prepare_env = dict(raw.get("prepare_env") or {})
    verify_env = dict(raw.get("verify_env") or {})
    image = str(raw.get("image") or "")
    command = str(raw.get("command") or "exec")
    if command not in ("exec", "workflow", "goal"):
        raise ScenarioError(f"{path}: command must be exec, workflow or goal")
    script = str(raw.get("script") or "workflow.js")
    prepare = None
    if raw.get("prepare"):
        prepare_path = (path.parent / raw["prepare"]).resolve()
        if not prepare_path.is_file():
            raise ScenarioError(f"{path}: prepare script not found: {prepare_path}")
        prepare = prepare_path
    execution = dict(raw.get("execution") or {})
    from .evidence import validate_execution
    try:
        validate_execution(execution)
    except ValueError as error:
        raise ScenarioError(f"{path}: {error}") from error
    evaluation = dict(raw.get("evaluation") or {})
    if evaluation.get("split", "unknown") not in {"holdout", "tuning", "unknown"}:
        raise ScenarioError(f"{path}: split must be holdout, tuning or unknown")
    constraints = list(raw.get("evidence") or [])
    seen_evidence = set()
    for constraint in constraints:
        if not isinstance(constraint, dict):
            raise ScenarioError(f"{path}: evidence must be an object")
        if not all(isinstance(constraint.get(key), str) and constraint[key] for key in ("id", "source", "contains")):
            raise ScenarioError(f"{path}: evidence needs id, source and contains")
        if constraint["id"] in seen_evidence:
            raise ScenarioError(f"{path}: duplicate evidence id {constraint['id']}")
        seen_evidence.add(constraint["id"])
    readonly_inputs = raw.get("readonly_inputs", [])
    if not isinstance(readonly_inputs, list) or any(not isinstance(p, str) or not p or Path(p).is_absolute() or ".." in Path(p).parts for p in readonly_inputs):
        raise ScenarioError(f"{path}: readonly_inputs must be workspace-relative paths")
    verifier_root = (root / raw["verifier_root"]).resolve() if raw.get("verifier_root") else None
    if verifier_root is not None and (not verifier_root.is_dir() or verifier_root.is_relative_to(fixture)):
        raise ScenarioError(f"{path}: verifier_root must be a directory outside the fixture")
    if not raw["verify"] or any(not isinstance(c, str) or not c.strip() for c in raw["verify"]):
        raise ScenarioError(f"{path}: verify must contain nonempty commands")
    return Scenario(
        id=raw["id"],
        axis=raw["axis"],
        rung=raw["rung"],
        difficulty=raw["difficulty"],
        backend=raw["backend"],
        fixture=fixture,
        verify=list(raw["verify"]),
        prompt=prompt,
        limits=dict(raw.get("limits") or {}),
        anti_cheat=list(raw.get("anti_cheat") or []),
        diagnostics=list(raw.get("diagnostics") or []),
        provider=str(raw.get("provider") or "mock"),
        prepare=prepare,
        prepare_env=prepare_env,
        verify_env=verify_env,
        image=image,
        docker_args=list(raw.get("docker_args") or []),
        workspace_mode=str(raw.get("workspace_mode") or "rw"),
        command=command,
        script=script,
        path=path, execution=execution, evaluation=evaluation, evidence=constraints,
        readonly_inputs=list(readonly_inputs), verifier_root=verifier_root,
    )


def discover(root: Path) -> list[Scenario]:
    """Every `scenario.json` under `root`, sorted by axis and rung."""
    found, ids = [], set()
    for path in sorted(Path(root).rglob("scenario.json")):
        item = load(path)
        if item.id in ids:
            raise ScenarioError(f"{path}: duplicate scenario id {item.id}")
        ids.add(item.id)
        found.append(item)
    return sorted(found, key=lambda s: (s.axis, s.rung, s.id))


def select(
    scenarios: list[Scenario],
    axes: list[str] | None = None,
    rungs: tuple[int, int] | None = None,
    ids: list[str] | None = None,
) -> list[Scenario]:
    chosen = scenarios
    if axes:
        chosen = [s for s in chosen if s.axis in axes]
    if rungs:
        low, high = rungs
        chosen = [s for s in chosen if low <= s.rung <= high]
    if ids:
        wanted = set(ids)
        chosen = [s for s in chosen if s.id in wanted]
        missing = wanted - {s.id for s in chosen}
        if missing:
            raise ScenarioError(f"unknown scenario id(s): {', '.join(sorted(missing))}")
    return chosen
