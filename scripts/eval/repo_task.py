#!/usr/bin/env python3
"""Repo-fix evaluation harness (L3-lite): real bug fixes from this repo's history.

**Host caveat.** The repo's `tests/*_contract.rs` suites start real shells through
the execution broker, which requires an OS-enforced sandbox backend. Inside an
already-sandboxed process tree (this agent session, and any CI that sandboxes the
job) the macOS seatbelt probe dies with ``no OS-enforced sandbox backend on this
host: seatbelt probe terminated by signal 6``, so *every* verification fails —
including the reference (gold) patch. `verify_tests_passed` is therefore forced to
False and flagged via ``verification_environment_unsupported`` in that case; run
the harness from a normal shell or inside a Linux container to get real verdicts.

Builds SWE-bench-style tasks out of the repository's own `fix(...)` commits:

    base  = <commit>^                (the buggy tree)
    issue = the commit subject/body  (what the agent is told)
    check = the test files that commit changed, restored over the agent's tree
            and run with cargo nextest

That gives FAIL_TO_PASS semantics without any external dataset, and it measures
Orca on real repository work (multi-file Rust changes, real tests).

Usage:
    python3 scripts/eval/repo_task.py list [--limit 10]
    python3 scripts/eval/repo_task.py prepare <sha>
    python3 scripts/eval/repo_task.py run <sha> [--timeout 1800]
    python3 scripts/eval/repo_task.py report
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
WORKTREES = REPO / ".worktrees"
RESULTS = REPO / "jobs" / "eval-repo-tasks"


def git(*args: str, cwd: Path | None = None) -> str:
    return subprocess.run(
        ["git", *args], cwd=str(cwd or REPO), capture_output=True, text=True, check=True
    ).stdout


def ok_git(*args: str, cwd: Path | None = None) -> bool:
    return (
        subprocess.run(
            ["git", *args], cwd=str(cwd or REPO), capture_output=True, text=True
        ).returncode
        == 0
    )


def commit_info(sha: str) -> dict:
    subject = git("log", "-1", "--pretty=%s", sha).strip()
    body = git("log", "-1", "--pretty=%b", sha).strip()
    files = [f for f in git("show", "--name-only", "--pretty=", sha).splitlines() if f]
    crates = sorted(
        {
            match.group(1)
            for path in files
            if (match := re.match(r"crates/([^/]+)/", path))
        }
    )
    test_files = [
        path
        for path in files
        if re.search(r"(^|/)tests?/|_test\.rs$|test_[^/]*\.rs$", path)
    ]
    return {
        "sha": sha,
        "subject": subject,
        "body": body,
        "files": files,
        "crates": crates,
        "test_files": test_files,
    }


def benchmark_running() -> bool:
    """True when a Harbor/Terminal-Bench trial container is alive.

    A cargo build steals cores from wall-clock-budgeted trials, which shows up as
    spurious timeouts (observed 2026-09-16), so `run` refuses to start while one
    is active unless --force is given.
    """
    result = subprocess.run(
        ["docker", "ps", "--filter", "name=__env-main-1", "--format", "{{.Names}}"],
        capture_output=True,
        text=True,
    )
    return bool(result.stdout.strip())


def candidates(limit: int) -> list[dict]:
    shas = [
        line.split()[0]
        for line in git("log", "--pretty=%h %s", "-400").splitlines()
        if re.match(r"^[0-9a-f]+ fix(\(|:)", line)
    ]
    out = []
    for sha in shas:
        info = commit_info(sha)
        if not info["crates"] and not any(f.startswith("src/") for f in info["files"]):
            continue
        if not info["test_files"]:
            continue
        if any(f.startswith(("site/", "npm/", "docs/")) for f in info["files"]):
            continue
        out.append(info)
        if len(out) >= limit:
            break
    return out


def worktree_for(sha: str) -> Path:
    return WORKTREES / f"eval-{sha[:10]}"


def failing_tests(output: str) -> set[str]:
    """Test names the runner reported as FAILED (cargo test and nextest formats).

    Handles `test <name> ... FAILED`, nextest's `FAIL [ 1.2s] (1/2) <name>` and
    the names listed under cargo's `failures:` section.
    """
    names: set[str] = set()
    in_failures = False
    for line in output.splitlines():
        stripped = line.strip()
        if stripped.startswith("test result:"):
            in_failures = False
            continue
        if stripped in ("failures:", "---- failures ----"):
            in_failures = True
            continue
        if stripped.startswith("test ") and " FAILED" in stripped:
            names.add(stripped.split()[1])
            continue
        if "FAIL [" in stripped and "]" in stripped:
            after = stripped.split("]", 1)[1].strip()
            if after:
                names.add(after.split()[0].rstrip("."))
            continue
        if stripped.startswith("----"):
            in_failures = False
            continue
        # Rust test names never contain spaces; this filters panic text that
        # cargo prints between the `failures:` sections.
        if in_failures and re.fullmatch(r"[A-Za-z0-9_:]+", stripped):
            names.add(stripped)
    return {name for name in names if name and ":" not in name}


def hanging_tests(output: str) -> set[str]:
    """Test names libtest reported as still running when the run was killed.

    `test <name> has been running for over 60 seconds` means the test never reached a
    verdict — at base that is a failure mode just as real as `FAILED`, and it is how
    `tui_pty_contract` behaves before `c178c96e6` (six tests hang, the target is killed
    by `timeout`). Counting them keeps partial progress (a hang turned into a pass)
    visible instead of scoring the whole target as "no fixed tests".
    """
    names: set[str] = set()
    for line in output.splitlines():
        stripped = line.strip()
        if stripped.startswith("test ") and " has been running for over " in stripped:
            names.add(stripped.split()[1])
    return {name for name in names if name and ":" not in name}


def compile_failed_targets(output: str) -> set[str]:
    """Test targets cargo could not build, e.g. `could not compile `x` (test "y")`.

    A fix that changes a public API makes the *reference* tests unbuildable at the parent
    commit. Those tests are still valid FAIL_TO_PASS candidates (they cannot pass at base),
    but their names never appear in the base output, so scoring has to fall back to the
    tests that pass at the fix commit for the same targets.
    """
    return set(re.findall(r"could not compile `[^`]+` \(test \"([^\"]+)\"\)", output))


def split_streams(output: str) -> tuple[str, str]:
    """Split a captured `stdout + stderr` blob back into its two streams.

    Cargo writes progress (`Compiling` / `Finished` / `Running`) to stderr and libtest writes
    results to stdout; every artifact this harness saves is `stdout + stderr`, so the first
    cargo progress line marks the boundary.
    """
    lines = output.splitlines()
    for index, line in enumerate(lines):
        stripped = line.lstrip()
        if stripped.startswith(("Compiling ", "Finished ", "Running ", "Blocking ", "Fresh ")):
            return "\n".join(lines[:index]), "\n".join(lines[index:])
    return output, ""


def tests_by_target(output: str, only_passing: bool = True) -> dict[str, set[str]]:
    """Attribute libtest results to cargo test targets.

    The target order comes from stderr's `Running tests/<name>.rs` lines; the per-target
    results are the stdout blocks closed by a `test result:` line. Cargo runs targets in
    order, so the i-th block belongs to the i-th target.
    """
    stdout, stderr = split_streams(output)
    targets: list[str] = []
    for match in re.finditer(r"Running (?:unittests )?(?:tests[/\\])?([\w./-]+?)(?:\.rs)? \(target", stderr):
        name = match.group(1)
        name = name.rsplit("/", 1)[-1]
        targets.append(name)
    blocks: list[set[str]] = []
    current: set[str] = set()
    for line in stdout.splitlines():
        stripped = line.strip()
        if stripped.startswith("test ") and stripped.endswith(" ... ok"):
            current.add(stripped.split()[1])
        elif stripped.startswith("test ") and stripped.endswith(" ... FAILED"):
            current.add(stripped.split()[1])
        elif stripped.startswith("test result:"):
            blocks.append(current)
            current = set()
    if current:
        blocks.append(current)
    by_target: dict[str, set[str]] = {}
    for index, target in enumerate(targets):
        if index < len(blocks):
            by_target.setdefault(target, set()).update(blocks[index])
    return by_target


def passing_tests(output: str) -> set[str]:
    """Test names the runner reported as passing (`test <name> ... ok`)."""
    names: set[str] = set()
    for line in output.splitlines():
        stripped = line.strip()
        if stripped.startswith("test ") and stripped.endswith(" ... ok"):
            names.add(stripped.split()[1])
    return {name for name in names if name and ":" not in name}


def shell_command(
    command: str,
    tree: Path,
    container: str | None,
    workdir: str,
    env_passthrough: dict[str, str] | None = None,
) -> list[str]:
    """Wrap a shell command for the host or for a running verification container.

    Container runs use a non-login shell so the image's PATH (cargo, orca) is kept,
    and selected env vars are forwarded explicitly — `docker exec` does not inherit
    the caller's environment.
    """
    if container:
        flags: list[str] = []
        for key, value in (env_passthrough or {}).items():
            flags += ["-e", f"{key}={value}"]
        return ["docker", "exec", "-i", *flags, container, "sh", "-c", f"cd {workdir} && {command}"]
    return ["bash", "-lc", f"cd {tree} && {command}"]


def api_key() -> str | None:
    auth = Path.home() / ".orca" / "auth.json"
    if not auth.exists():
        return None
    try:
        return json.loads(auth.read_text(encoding="utf-8")).get("DEEPSEEK_API_KEY")
    except (OSError, json.JSONDecodeError):
        return None


def prepare(sha: str, worktree_override: str | None = None) -> dict:
    """Check out the fix's parent commit in a worktree (fresh or an existing one).

    Reusing one worktree across tasks keeps a single cargo target directory (and,
    in container mode, a single bind mount), so registry dependencies are compiled
    once instead of per task.
    """
    info = commit_info(sha)
    if worktree_override:
        tree = Path(worktree_override).resolve()
        subprocess.run(["git", "reset", "--hard", f"{sha}^"], cwd=str(tree), check=True,
                       capture_output=True)
        subprocess.run(["git", "checkout", "--detach", f"{sha}^"], cwd=str(tree), check=True,
                       capture_output=True)
        return {**info, "worktree": str(tree), "created": False, "reused": True}
    tree = worktree_for(sha)
    if tree.exists():
        return {**info, "worktree": str(tree), "created": False}
    WORKTREES.mkdir(exist_ok=True)
    git("worktree", "add", "--detach", str(tree), f"{sha}^")
    return {**info, "worktree": str(tree), "created": True}


def verification_commands(info: dict) -> list[list[str]]:
    """One command per package, so root and crate test targets never mix."""
    base = ["nice", "-n", "15", "cargo", "nextest", "run"]
    tail = ["--no-fail-fast", "--retries", "0"]
    grouped: dict[str | None, list[str]] = {}
    for path in info["test_files"]:
        if not re.search(r"(^|/)tests/", path):
            continue
        crate = None
        if match := re.match(r"crates/([^/]+)/", path):
            crate = match.group(1)
        target = f"--test {Path(path).stem}"
        targets = grouped.setdefault(crate, [])
        if target not in targets:
            targets.append(target)
    commands = []
    for crate, targets in grouped.items():
        prefix = ["-p", crate] if crate else []
        commands.append([*base, *prefix, *targets, *tail])
    if commands:
        return commands
    packages: list[str] = []
    for crate in info["crates"]:
        packages += ["-p", crate, "--lib"]
    if packages:
        return [[*base, *packages, *tail]]
    return [[*base, "--workspace", *tail]]


def verification_shell(info: dict) -> str:
    return " && ".join(" ".join(cmd) for cmd in verification_commands(info))


def verification_command(info: dict) -> list[str]:
    """Run the test targets the fix touched (fallback: the crate's lib tests).

    Targeting changed test files keeps verification to a FAIL_TO_PASS subset
    instead of the whole crate suite, which matters because every task reruns it.
    """
    # nice keeps an evaluation run from stealing cores from a benchmark run.
    base = ["nice", "-n", "15", "cargo", "nextest", "run"]
    tail = ["--no-fail-fast", "--retries", "0"]
    args: list[str] = []
    seen: list[str] = []
    for path in info["test_files"]:
        if not re.search(r"(^|/)tests/", path):
            continue
        crate = None
        if match := re.match(r"crates/([^/]+)/", path):
            crate = match.group(1)
        if crate:
            args += ["-p", crate]
        target = f"--test {Path(path).stem}"
        if target not in seen:
            args.append(target)
            seen.append(target)
    if args:
        return [*base, *args, *tail]
    packages: list[str] = []
    for crate in info["crates"]:
        packages += ["-p", crate, "--lib"]
    if packages:
        return [*base, *packages, *tail]
    return [*base, "--workspace", *tail]


GOLD_DIR = RESULTS / "gold"


def gold_result_path(sha: str) -> Path:
    return GOLD_DIR / f"{sha}.json"


def load_gold(sha: str) -> dict | None:
    """FAIL_TO_PASS/PASS_TO_PASS measured on the *fix commit* (see `gold_reference`)."""
    path = gold_result_path(sha)
    if not path.exists():
        return None
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except json.JSONDecodeError:
        return None


def gold_reference(
    sha: str,
    build_timeout: float = 3600.0,
    container: str | None = None,
    container_workdir: str = "/worktree",
    verify_command_override: str | None = None,
    worktree_override: str | None = None,
    verify_timeout: float | None = None,
) -> dict:
    """Run the reference tests against the fix commit itself.

    SWE-bench scoring needs to know which tests the *fix* is responsible for: tests that
    do not pass at the parent commit but do pass at the fix commit (FAIL_TO_PASS), while
    tests that already passed at the parent must keep passing (PASS_TO_PASS). Without
    this, a pre-existing failure or a test that merely *hangs* at base is
    indistinguishable from the fix's target, and partial progress (a hang turned into a
    pass) is scored as nothing.
    """
    info = prepare(sha, worktree_override)
    tree = Path(info["worktree"])
    GOLD_DIR.mkdir(parents=True, exist_ok=True)

    limited_env = dict(os.environ)
    limited_env["CARGO_BUILD_JOBS"] = "6"
    if shared := os.environ.get("ORCA_EVAL_TARGET_DIR"):
        limited_env["CARGO_TARGET_DIR"] = shared
    verify_command = verify_command_override or verification_shell(info)
    if container and not verify_command_override:
        verify_command = (
            verify_command.replace("nice -n 15 ", "")
            .replace("cargo nextest run", "cargo test")
            .replace("--no-fail-fast", "")
            .replace("--retries 0", "")
        )
        verify_command = " ".join(verify_command.split())
    if not verify_command_override:
        verify_command = f"timeout {int(verify_timeout or 900)} {verify_command}"

    # Gold source + reference tests, then back to the base the agent phase expects.
    subprocess.run(["git", "checkout", sha, "--", "."], cwd=str(tree), check=True, capture_output=True)
    try:
        run_result = subprocess.run(
            shell_command(verify_command, tree, container, container_workdir),
            capture_output=True,
            text=True,
            timeout=build_timeout,
            env=limited_env,
        )
        output = (run_result.stdout or "") + (run_result.stderr or "")
        timed_out = run_result.returncode == 124
    finally:
        subprocess.run(["git", "reset", "--hard", f"{sha}^"], cwd=str(tree), check=True, capture_output=True)
        subprocess.run(["git", "checkout", "--detach", f"{sha}^"], cwd=str(tree), check=True, capture_output=True)

    failing = failing_tests(output)
    hanging = hanging_tests(output)
    passing = passing_tests(output)
    result = {
        "sha": sha,
        "subject": info["subject"],
        "worktree": str(tree),
        "verify_command": verify_command,
        "gold_failures": sorted(failing),
        "gold_hanging": sorted(hanging),
        "gold_passing": sorted(passing),
        "gold_passing_by_target": {target: sorted(tests) for target, tests in tests_by_target(output).items()},
        "gold_not_passing": sorted(failing | hanging),
        "gold_run_timed_out": timed_out,
    }
    gold_result_path(sha).write_text(json.dumps(result, indent=2), encoding="utf-8")
    (GOLD_DIR / f"{sha}-tests.txt").write_text(output[-20000:], encoding="utf-8")
    return result


def run(
    sha: str,
    timeout: float,
    binary: str,
    model_note: str = "",
    build_timeout: float = 3600.0,
    container: str | None = None,
    container_workdir: str = "/worktree",
    verify_command_override: str | None = None,
    worktree_override: str | None = None,
    verify_timeout: float | None = None,
    baseline_only: bool = False,
) -> dict:
    info = prepare(sha, worktree_override)
    tree = Path(info["worktree"])
    RESULTS.mkdir(parents=True, exist_ok=True)
    stamp = time.strftime("%Y%m%d-%H%M%S")
    out_dir = RESULTS / f"{sha[:10]}-{stamp}"
    out_dir.mkdir(parents=True, exist_ok=True)

    issue = f"{info['subject']}\n\n{info['body']}".strip()
    prompt = (
        f"This checkout has a bug.\n\n{issue}\n\n"
        "Find the root cause and fix it in this working tree. "
        "Do not edit the tests; the fix must make the existing tests pass."
    )
    (out_dir / "issue.md").write_text(prompt, encoding="utf-8")

    # Baseline: the tests the fix touched must fail (or not exist) before the fix.
    limited_env = dict(os.environ)
    limited_env["CARGO_BUILD_JOBS"] = "6"
    # Optional shared target dir (registry dependencies are then compiled once
    # for all worktrees). Local crates still rebuild per worktree because cargo
    # fingerprints include the source path. Set ORCA_EVAL_TARGET_DIR to enable.
    if shared := os.environ.get("ORCA_EVAL_TARGET_DIR"):
        limited_env["CARGO_TARGET_DIR"] = shared
    verify_command = verify_command_override or verification_shell(info)
    if container and not verify_command_override:
        # The verification image has no cargo-nextest; translate the default
        # command into its cargo-test equivalent (nextest-only flags dropped).
        verify_command = (
            verify_command.replace("nice -n 15 ", "")
            .replace("cargo nextest run", "cargo test")
            .replace("--no-fail-fast", "")
            .replace("--retries 0", "")
        )
        verify_command = " ".join(verify_command.split())
    if not verify_command_override:
        # A hung test target must not block the run for the whole build budget:
        # bound each verification invocation, mirroring the repo's own CI, which
        # gets this from nextest's slow-timeout ({ period = "60s", terminate-after = 2 }).
        verify_timeout_s = int(verify_timeout or 900)
        verify_command = f"timeout {verify_timeout_s} {verify_command}"

    # FAIL_TO_PASS baseline: the *reference* tests, run against the unfixed source.
    # The agent never sees them: they are reverted before the agent phase.
    subprocess.run(["git", "checkout", "--", *info["test_files"]], cwd=str(tree))
    subprocess.run(["git", "checkout", sha, "--", *info["test_files"]], cwd=str(tree))
    baseline = subprocess.run(
        shell_command(verify_command, tree, container, container_workdir),
        capture_output=True,
        text=True,
        timeout=build_timeout,
        env=limited_env,
    )
    baseline_output = (baseline.stdout or "") + (baseline.stderr or "")
    (out_dir / "baseline-tests.txt").write_text(baseline_output[-20000:], encoding="utf-8")
    baseline_failures = failing_tests(baseline_output)
    baseline_hanging = hanging_tests(baseline_output)
    baseline_not_passing = baseline_failures | baseline_hanging
    subprocess.run(["git", "checkout", "--", *info["test_files"]], cwd=str(tree))

    if baseline_only:
        broken_targets = compile_failed_targets(baseline_output)
        summary = {
            "sha": sha,
            "subject": info["subject"],
            "worktree": str(tree),
            "verify_command": verify_command,
            "baseline_failures": sorted(baseline_failures),
            "baseline_hanging": sorted(baseline_hanging),
            "baseline_not_passing": sorted(baseline_not_passing),
            "baseline_run_timed_out": baseline.returncode == 124,
            "baseline_compile_failed_targets": sorted(broken_targets),
            # Reference tests that do not build at the parent commit are still valid
            # targets: their names come from the gold run instead (see scoring below).
            "fail_to_pass_on_this_platform": bool(baseline_not_passing or broken_targets),
            "artifacts": str(out_dir),
        }
        (out_dir / "result.json").write_text(json.dumps(summary, indent=2), encoding="utf-8")
        return summary


    env = dict(os.environ)
    env["PATH"] = f"{Path(binary).resolve().parent}{os.pathsep}{env.get('PATH','')}"
    if container and (key := api_key()):
        env["DEEPSEEK_API_KEY"] = key
    started = time.time()
    # Popen + communicate (not subprocess.run) so a timed-out agent still yields its
    # partial trajectory: `run()` discards captured output when it kills the child, and
    # two of the runs here hit the wall-clock budget before this was fixed.
    agent_command = shell_command(
        f"orca exec --mode full-auto --output-format jsonl --no-history -- {json.dumps(prompt)}",
        tree,
        container,
        container_workdir,
        env_passthrough=(
            {"DEEPSEEK_API_KEY": env["DEEPSEEK_API_KEY"]}
            if container and env.get("DEEPSEEK_API_KEY")
            else None
        ),
    )
    agent_process = subprocess.Popen(
        agent_command,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=env,
    )
    try:
        agent_stdout, agent_stderr = agent_process.communicate(timeout=timeout)
        agent_code = agent_process.returncode
    except subprocess.TimeoutExpired:
        agent_process.kill()
        agent_stdout, agent_stderr = agent_process.communicate()
        agent_code = None
    (out_dir / "trajectory.jsonl").write_text(agent_stdout or "", encoding="utf-8")
    (out_dir / "agent-stderr.txt").write_text((agent_stderr or "")[-20000:], encoding="utf-8")
    agent_wall = round(time.time() - started, 1)

    diff = git("diff", cwd=tree)
    (out_dir / "agent.patch").write_text(diff, encoding="utf-8")

    # Restore the reference tests over the agent's tree, then verify.
    subprocess.run(["git", "checkout", sha, "--", *info["test_files"]], cwd=str(tree))
    check = subprocess.run(
        shell_command(verify_command, tree, container, container_workdir),
        capture_output=True,
        text=True,
        timeout=build_timeout,
        env=limited_env,
    )
    verify_output = (check.stdout or "") + (check.stderr or "")
    (out_dir / "verify-tests.txt").write_text(verify_output[-20000:], encoding="utf-8")
    verify_failures = failing_tests(verify_output)
    verify_hanging = hanging_tests(verify_output)
    verify_passing = passing_tests(verify_output)
    verify_not_passing = verify_failures | verify_hanging
    verify_by_target = tests_by_target(verify_output)
    baseline_passing = passing_tests(baseline_output)

    gold = load_gold(sha)
    if gold:
        # SWE-bench style: score against the tests the fix itself is responsible for.
        gold_not_passing = set(gold.get("gold_not_passing") or [])
        gold_passing = set(gold.get("gold_passing") or [])
        broken_targets = compile_failed_targets(baseline_output)
        if broken_targets:
            # The reference tests cannot even build at base: every test that passes at gold
            # in those targets is a legitimate FAIL_TO_PASS entry.
            by_target = gold.get("gold_passing_by_target") or {}
            from_gold = {test for target, tests in by_target.items() if target in broken_targets for test in tests}
            fail_to_pass = sorted(set(from_gold) | (baseline_not_passing - gold_not_passing))
        else:
            fail_to_pass = sorted(baseline_not_passing - gold_not_passing)
        pass_to_pass = sorted((baseline_passing & gold_passing) - gold_not_passing)
        fixed = sorted(set(fail_to_pass) & (verify_passing | {t for tests in verify_by_target.values() for t in tests}))
        regressions = sorted((set(fail_to_pass) | set(pass_to_pass)) & verify_not_passing)
        new_failures = regressions
        solved = bool(fail_to_pass) and not regressions and set(fail_to_pass) <= verify_passing
        scoring = "gold"
    else:
        fail_to_pass = sorted(baseline_not_passing)
        pass_to_pass = sorted(baseline_passing)
        # "fixed" = not passing at base (failed *or* hung), passing now.
        fixed = sorted(baseline_not_passing - verify_not_passing)
        new_failures = sorted(verify_not_passing - baseline_not_passing)
        solved = bool(fixed) and not new_failures
        scoring = "baseline-only (run `gold` for SWE-bench style scoring)"

    environment_unsupported = "no OS-enforced sandbox backend" in verify_output
    result = {
        "sha": sha,
        "verification_environment_unsupported": environment_unsupported,
        "subject": info["subject"],
        "crates": info["crates"],
        "test_files": info["test_files"],
        "worktree": str(tree),
        "container": container,
        "agent_exit_code": agent_code,
        "agent_wall_s": agent_wall,
        "agent_diff_bytes": len(diff),
        "agent_diff_files": sorted(
            {line.split(" b/")[-1] for line in diff.splitlines() if line.startswith("diff --git")}
        ),
        "baseline_tests_passed": baseline.returncode == 0,
        "verify_tests_passed": check.returncode == 0 and not environment_unsupported,
        "baseline_failures": sorted(baseline_failures),
        "baseline_hanging": sorted(baseline_hanging),
        "baseline_run_timed_out": baseline.returncode == 124,
        "verify_failures": sorted(verify_failures),
        "verify_hanging": sorted(verify_hanging),
        "verify_run_timed_out": check.returncode == 124,
        "fail_to_pass": fail_to_pass,
        "pass_to_pass": pass_to_pass,
        "fixed_tests": fixed,
        "new_failures": new_failures,
        "scoring": scoring,
        "baseline_compile_failed_targets": sorted(compile_failed_targets(baseline_output)),
        "solved": solved and not environment_unsupported,
        "verify_command": verify_command,
        "model_note": model_note,
        "artifacts": str(out_dir),
    }
    (out_dir / "result.json").write_text(json.dumps(result, indent=2), encoding="utf-8")
    return result


def effective_fail_to_pass(sha: str) -> list[str]:
    """The FAIL_TO_PASS set a `run` would score against, given the recorded artifacts.

    Empty means the candidate cannot demonstrate anything: either its reference tests do
    not build at base *and* nothing passes at gold in those targets, or every test that
    fails at base also fails at the fix commit (a pre-existing flake — e.g.
    `server_mode_streams_workflow_item_lifecycle` fails at both ends).
    """
    gold = load_gold(sha) or {}
    gold_not_passing = set(gold.get("gold_not_passing") or [])
    baseline_not_passing: set[str] = set()
    baseline_passing: set[str] = set()
    baseline_output = ""
    # `run()` stores artifacts under a ten-character SHA prefix.
    for path in sorted(RESULTS.glob(f"{sha[:10]}-*/result.json"), reverse=True):
        row = json.loads(path.read_text(encoding="utf-8"))
        if "baseline_not_passing" in row or "baseline_failures" in row:
            baseline_not_passing = set(row.get("baseline_not_passing") or row.get("baseline_failures") or [])
            baseline_passing = set(row.get("baseline_passing") or [])
            baseline_output = (path.parent / "baseline-tests.txt").read_text(encoding="utf-8") if (path.parent / "baseline-tests.txt").exists() else ""
            break
    broken_targets = compile_failed_targets(baseline_output)
    if broken_targets:
        by_target = gold.get("gold_passing_by_target") or {}
        from_gold = {test for target, tests in by_target.items() if target in broken_targets for test in tests}
        return sorted(set(from_gold) | (baseline_not_passing - gold_not_passing))
    return sorted(baseline_not_passing - gold_not_passing)


def report() -> int:
    """Tally every run, tolerating the three result schemas this harness has produced."""
    rows: list[dict] = []
    for path in sorted(RESULTS.glob("*")):
        result = path / "result.json"
        if not result.exists():
            continue
        row = json.loads(result.read_text(encoding="utf-8"))
        rescored = path / "result-rescored.json"
        if rescored.exists():
            row = {**row, **json.loads(rescored.read_text(encoding="utf-8")), "rescored": True}
        row["_dir"] = path.name
        rows.append(row)
    if not rows:
        print("no repo-task runs yet")
        return 1

    def mode(row: dict) -> str:
        if row.get("agent_wall_s") is None and "baseline_failures" in row:
            return "baseline-only"
        if row.get("agent_wall_s") is None:
            return "unfinished"
        return "run"

    print(f"{'sha':<11}{'mode':<14}{'solved':<8}{'fixed':>6}{'new':>5}{'f2p':>5}"
          f"{'diff B':>8}{'wall s':>8}  subject")
    scored = 0
    solved = 0
    for row in rows:
        kind = mode(row)
        f2p = row.get("fail_to_pass") or row.get("baseline_not_passing") or row.get("baseline_failures") or []
        fixed = row.get("fixed_tests") or []
        new = row.get("new_failures") or []
        is_solved = row.get("solved")
        # Rows written before gold scoring existed have no `scoring` field and no reliable
        # verdict; list them but keep them out of the tally.
        countable = kind == "run" and (row.get("scoring") is not None or row.get("rescored"))
        if countable:
            scored += 1
            solved += 1 if is_solved else 0
        print(
            f"{str(row.get('sha', '?'))[:10]:<11}{kind:<14}{str(is_solved):<8}{len(fixed):>6}"
            f"{len(new):>5}{len(f2p):>5}{str(row.get('agent_diff_bytes', '-')):>8}"
            f"{str(row.get('agent_wall_s', '-')):>8}  {str(row.get('subject', ''))[:44]}"
            + ("  [rescored]" if row.get("rescored") else "")
        )
    print(f"\nagent runs scored: {solved}/{scored} solved (legacy rows without a scoring field are listed only)")
    golds = (
        sorted(g for g in (RESULTS / "gold").glob("*.json") if "." not in g.stem)
        if (RESULTS / "gold").exists()
        else []
    )
    print(f"gold references measured: {len(golds)} ({', '.join(g.stem[:10] for g in golds)})")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser()
    sub = parser.add_subparsers(dest="command", required=True)
    list_parser = sub.add_parser("list")
    list_parser.add_argument("--limit", type=int, default=10)
    ftp_parser = sub.add_parser("fail-to-pass", help="print the effective FAIL_TO_PASS set for a sha")
    ftp_parser.add_argument("sha")
    ftp_parser.add_argument("--count", action="store_true", help="print only the number of tests")

    gold_parser = sub.add_parser("gold", help="measure FAIL_TO_PASS/PASS_TO_PASS on the fix commit")
    gold_parser.add_argument("sha")
    gold_parser.add_argument("--worktree", default=None)
    gold_parser.add_argument("--container", default=None)
    gold_parser.add_argument("--container-workdir", default="/worktree")
    gold_parser.add_argument("--verify-command", default=None)
    gold_parser.add_argument("--verify-timeout", type=float, default=900.0)
    gold_parser.add_argument("--build-timeout", type=float, default=5400.0)
    # accepted for parity with `run` (the worktree is always reset to the base first)
    gold_parser.add_argument("--force", action="store_true")

    for name in ("prepare", "run"):
        sub_parser = sub.add_parser(name)
        sub_parser.add_argument("sha")
        if name == "run":
            sub_parser.add_argument("--timeout", type=float, default=1800.0)
            sub_parser.add_argument("--build-timeout", type=float, default=5400.0)
            sub_parser.add_argument("--force", action="store_true")
            sub_parser.add_argument("--container", default=None, help="run agent+verification inside this container")
            sub_parser.add_argument("--container-workdir", default="/worktree")
            sub_parser.add_argument(
                "--baseline-only",
                action="store_true",
                help="only measure FAIL_TO_PASS on this platform (no agent run)",
            )
            sub_parser.add_argument(
                "--verify-timeout",
                type=float,
                default=900.0,
                help="seconds before a single verification invocation is killed (default 900)",
            )
            sub_parser.add_argument(
                "--worktree",
                default=None,
                help="reuse this worktree (reset to the fix's parent) instead of creating one",
            )
            sub_parser.add_argument(
                "--verify-command",
                default=None,
                help="override the verification command (e.g. cargo test when nextest is unavailable)",
            )
            sub_parser.add_argument("--binary", default="target/release/orca")
    sub.add_parser("report")
    args = parser.parse_args()

    if args.command == "fail-to-pass":
        names = effective_fail_to_pass(args.sha)
        print(len(names) if args.count else "\n".join(names))
        return 0

    if args.command == "gold":
        result = gold_reference(
            args.sha,
            build_timeout=args.build_timeout,
            container=args.container,
            container_workdir=args.container_workdir,
            verify_command_override=args.verify_command,
            worktree_override=args.worktree,
            verify_timeout=args.verify_timeout,
        )
        print(json.dumps(result, indent=2))
        return 0

    if args.command == "list":
        for info in candidates(args.limit):
            print(
                f"{info['sha'][:10]}  crates={','.join(info['crates']) or '-'} "
                f"tests={len(info['test_files'])}  {info['subject'][:70]}"
            )
        return 0
    if args.command == "prepare":
        info = prepare(args.sha)
        print(json.dumps(info, indent=2))
        return 0
    if args.command == "run":
        if benchmark_running() and not args.force:
            print(
                "a Harbor/Terminal-Bench trial is running; cargo would perturb its wall-clock "
                "budget. Re-run when it finishes, or pass --force.",
                file=sys.stderr,
            )
            return 3
        result = run(
            args.sha,
            args.timeout,
            args.binary,
            build_timeout=args.build_timeout,
            container=args.container,
            container_workdir=args.container_workdir,
            verify_command_override=args.verify_command,
            worktree_override=args.worktree,
            verify_timeout=args.verify_timeout,
            baseline_only=args.baseline_only,
        )
        print(json.dumps(result, indent=2))
        return 0
    return report()


if __name__ == "__main__":
    raise SystemExit(main())
