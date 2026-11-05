"""Small evidence helpers shared by the existing runner and backends (stdlib only)."""
from __future__ import annotations
import hashlib
import json
import math
import shlex
from pathlib import Path
from urllib.parse import urlsplit

UNKNOWN = "unknown"
EXECUTION_KEYS = ("model", "reasoning_effort", "endpoint", "max_turns", "max_tool_calls",
                  "max_cost_usd", "max_wall_time_secs")
BUDGET_KEYS = EXECUTION_KEYS[3:]


def digest(value) -> str:
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(",", ":")).encode()).hexdigest()


def file_sha(path: Path) -> str:
    try:
        return hashlib.sha256(path.read_bytes()).hexdigest()
    except OSError:
        return UNKNOWN


def tree_sha(root: Path) -> str:
    if not root.is_dir():
        return UNKNOWN
    entries = []
    try:
        for path in sorted(root.rglob("*")):
            if any(part in {".git", "__pycache__"} for part in path.relative_to(root).parts):
                continue
            if path.is_symlink():
                entries.append((str(path.relative_to(root)), "symlink", str(path.readlink())))
            elif path.is_file():
                sha = file_sha(path)
                if sha == UNKNOWN:
                    return UNKNOWN
                entries.append((str(path.relative_to(root)), sha))
    except OSError:
        return UNKNOWN
    return digest(entries)


def endpoint_identity(endpoint) -> str:
    """Origin only: userinfo, arbitrary paths, query and fragment are never persisted."""
    if not endpoint:
        return UNKNOWN
    try:
        parsed = urlsplit(endpoint)
        if parsed.scheme not in {"http", "https"} or not parsed.hostname:
            return UNKNOWN
        host = parsed.hostname
        if ":" in host:
            host = f"[{host}]"
        return f"{parsed.scheme}://{host}" + (f":{parsed.port}" if parsed.port else "")
    except ValueError:
        return UNKNOWN


def validate_execution(config: dict) -> None:
    if set(config) - set(EXECUTION_KEYS):
        raise ValueError("unsupported execution keys")
    for key, value in config.items():
        if key in {"model", "endpoint", "reasoning_effort"}:
            if not isinstance(value, str) or not value.strip():
                raise ValueError(f"{key} must be a nonempty string")
            if key == "reasoning_effort" and value not in {"low", "high", "max"}:
                raise ValueError("reasoning_effort must be low, high or max")
            if key == "endpoint" and endpoint_identity(value) == UNKNOWN:
                raise ValueError("endpoint must be an HTTP(S) URL")
        elif isinstance(value, bool) or not isinstance(value, (int, float)) or not math.isfinite(value) or value < 0:
            raise ValueError(f"{key} must be a finite nonnegative number")
        elif key != "max_cost_usd" and not isinstance(value, int):
            raise ValueError(f"{key} must be an integer")


def execution_config(item=None, args=None) -> dict:
    config = dict(getattr(item, "execution", {}) or {})
    for key in EXECUTION_KEYS:
        value = getattr(args, key, None)
        if value is not None:
            config[key] = value
    validate_execution(config)
    return config


def execution_protocol(config: dict) -> dict:
    model = config.get("model") or UNKNOWN
    return {"model": model, "requested_model": model,
            "reasoning_effort": config.get("reasoning_effort") or UNKNOWN,
            "endpoint_identity": endpoint_identity(config.get("endpoint")),
            "response_model": UNKNOWN, "provider_fingerprint": UNKNOWN,
            "engine_fingerprint": UNKNOWN, "tool_schema_sha256": UNKNOWN,
            "sampling": {key: UNKNOWN for key in ("temperature", "top_p", "seed")},
            "budget": {key: config.get(key, UNKNOWN) for key in BUDGET_KEYS}}


def exec_options(config: dict) -> list[str]:
    argv = []
    for key in ("model", *BUDGET_KEYS):
        if config.get(key) is not None:
            argv += ["--" + key.replace("_", "-"), str(config[key])]
    return argv


def verifier_paths(item, workspace: Path) -> list[Path]:
    paths = []
    for command in item.verify:
        for token in shlex.split(command):
            candidate = workspace / token
            # Commands are hashed separately; only actual file inputs count as verifier bytes.
            if not token.startswith("-") and candidate.is_file():
                paths.append(candidate)
    return sorted(set(paths))


def scenario_identity(item, workspace: Path | None = None) -> dict:
    workspace = workspace or item.fixture
    paths = verifier_paths(item, workspace)
    verifier = [(str(p.relative_to(workspace)) if p.is_relative_to(workspace) else str(p), file_sha(p)) for p in paths]
    tests = workspace / "tests"
    if tests.is_dir():
        verifier.append(("tests/", tree_sha(tests)))
    return {"scenario_sha256": file_sha(item.path) if item.path else UNKNOWN,
            "fixture_sha256": tree_sha(workspace), "source_fixture_sha256": tree_sha(item.fixture),
            "prompt_sha256": hashlib.sha256(item.prompt.encode()).hexdigest(),
            "prepare_sha256": file_sha(item.prepare) if item.prepare else UNKNOWN,
            "verifier_sha256": digest(verifier) if verifier and all(v != UNKNOWN for _, v in verifier) else UNKNOWN,
            "verifier_command_sha256": digest(item.verify)}


def provider_metadata(path: Path) -> dict:
    """Consume captured request/response JSONL; never infer response model from request/route."""
    models, responses, fingerprints, engines, schemas = set(), set(), set(), set(), set()
    sampling = {key: [] for key in ("temperature", "top_p", "seed")}
    efforts = set()
    if path.exists():
        for line in path.read_text(errors="replace").splitlines():
            try:
                record = json.loads(line)
            except (ValueError, TypeError):
                continue
            if not isinstance(record, dict):
                continue
            request = record.get("request", record)
            response = record.get("response", {})
            if isinstance(request, dict):
                if isinstance(request.get("model"), str): models.add(request["model"])
                if isinstance(request.get("reasoning_effort"), str): efforts.add(request["reasoning_effort"])
                for key in sampling:
                    if request.get(key) is not None and isinstance(request[key], (int, float)):
                        sampling[key].append(request[key])
                if isinstance(request.get("tools"), list): schemas.add(digest(request["tools"]))
            if isinstance(response, dict):
                if isinstance(response.get("model"), str): responses.add(response["model"])
                for key, target in (("system_fingerprint", fingerprints), ("engine_fingerprint", engines)):
                    if isinstance(response.get(key), str): target.add(response[key])
    def one(values):
        values = sorted(set(values))
        return values[0] if len(values) == 1 else values or UNKNOWN
    return {"requested_models": sorted(models), "response_models": sorted(responses),
            "provider_fingerprints": sorted(fingerprints), "engine_fingerprints": sorted(engines),
            "reasoning_efforts": sorted(efforts), "tool_schema_sha256": one(schemas),
            "sampling": {key: one(values) for key, values in sampling.items()}}


def attach_provider(protocol: dict, path: Path) -> dict:
    observed = provider_metadata(path)
    protocol = dict(protocol)
    for source, target in (("requested_models", "observed_requested_model"), ("response_models", "response_model"),
                           ("provider_fingerprints", "provider_fingerprint"), ("engine_fingerprints", "engine_fingerprint")):
        values = observed[source]
        protocol[target] = values[0] if len(values) == 1 else values or UNKNOWN
    protocol["observed_reasoning_effort"] = observed["reasoning_efforts"] or UNKNOWN
    protocol["sampling"] = observed["sampling"]
    protocol["tool_schema_sha256"] = observed["tool_schema_sha256"]
    return protocol


def suite_metadata(items, args=None) -> dict:
    rows = []
    for item in items:
        meta = dict(item.evaluation)
        for key in ("split", "suite_id"):
            requested = getattr(args, key, None)
            if requested is not None:
                if meta.get(key, UNKNOWN) not in {UNKNOWN, requested}:
                    raise ValueError(f"{item.id}: fixed {key} cannot be relabelled")
                meta[key] = requested
        rows.append({"id": item.id, "split": meta.get("split", UNKNOWN), "suite_id": meta.get("suite_id", UNKNOWN),
                     "identity": scenario_identity(item)})
    def common(key):
        values = sorted({row[key] for row in rows})
        return values[0] if len(values) == 1 else "mixed"
    return {"split": common("split"), "suite_id": common("suite_id"),
            "manifest_sha256": digest(rows), "scenarios": rows,
            "holdout_policy": "fixed manifest; never pool tuning or changed manifests"}
