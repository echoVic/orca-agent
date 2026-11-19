"""The right answer to each task in tasks.json, and whether an answer gives it.

The right answer is read from the checkout the question was asked about, not
stored: the questions count things in the source, and the source changes.
"""

from __future__ import annotations

import re
from pathlib import Path

RUNTIME_SRC = "crates/orca-runtime/src/"
RUST_PATH = re.compile(r"[\w./-]*\w\.rs")
NUMBER = re.compile(r"(?<![\w.])\d+(?!\w|\.\d)")


def _read(path: Path) -> str:
    return path.read_text(encoding="utf-8", errors="replace")


def _numbers(text: str) -> set[str]:
    return set(NUMBER.findall(text))


def _names(path: str, mentioned: str) -> bool:
    """Whether `mentioned`, a path as an answer wrote it, is `path`."""
    return path == mentioned or path.endswith("/" + mentioned)


def _only_these_files(answer: str, allowed: list[str]) -> bool:
    """Every Rust file the answer names is one of `allowed`."""
    return all(any(_names(path, mentioned) for path in allowed)
               for mentioned in RUST_PATH.findall(answer))


def _each_with_its_count(answer: str, counts: dict[str, int], under: str = "") -> bool:
    """Every path is on a line of the answer that also gives its count."""
    for path, count in counts.items():
        short = path.removeprefix(under)
        if not any(short in line and str(count) in _numbers(line.replace(short, " "))
                   for line in answer.splitlines()):
            return False
    return True


def _user_trusted_launches(answer: str, root: Path) -> bool:
    counts = {}
    for path in sorted((root / RUNTIME_SRC).rglob("*.rs")):
        count = _read(path).count("launch_user_trusted(")
        if count:
            counts[path.relative_to(root).as_posix()] = count
    return (_each_with_its_count(answer, counts, under=RUNTIME_SRC)
            and _only_these_files(answer, list(counts)))


def _workflow_events(answer: str, root: Path) -> bool:
    source = _read(root / "crates/orca-core/src/event_schema.rs")
    events = set(re.findall(r'"(workflow\.[a-z_.]+)"', source))
    named = {name.rstrip(".") for name in re.findall(r"workflow\.[a-z_.]+", answer)}
    return named == events and str(len(events)) in _numbers(answer)


def _network_tools(answer: str, root: Path) -> bool:
    source = _read(root / "crates/orca-tools/src/registry.rs")
    every = set(re.findall(r'_spec\(\s*"([a-z_]+)",', source))
    network = set(re.findall(
        r'_spec\(\s*"([a-z_]+)",(?:(?!_spec\().)*?CapabilitySet::network_search\(\)',
        source, re.S))
    named = set(re.findall(r"[a-z_]+", answer)) & every
    return bool(network) and named == network


def _largest_test_files(answer: str, root: Path) -> bool:
    counts = {path.relative_to(root).as_posix(): _read(path).count("#[test]")
              for path in (root / "tests").glob("*.rs")}
    largest = sorted(counts, key=lambda path: (-counts[path], path))[:5]
    if not _each_with_its_count(answer, {path: counts[path] for path in largest}):
        return False
    first_seen = [answer.find(path) for path in largest]
    return first_seen == sorted(first_seen)


def _credential_scrub_sites(answer: str, root: Path) -> bool:
    calling, defining = [], []
    for path in sorted((root / "crates").rglob("*.rs")):
        source = _read(path)
        name = path.relative_to(root).as_posix()
        if "fn remove_credential_env(" in source:
            defining.append(name)
        elif "remove_credential_env(" in source:
            calling.append(name)
    return (all(path in answer for path in calling)
            and _only_these_files(answer, calling + defining))


CHECKS = {
    "user-trusted-launches": _user_trusted_launches,
    "workflow-events": _workflow_events,
    "network-tools": _network_tools,
    "largest-test-files": _largest_test_files,
    "credential-scrub-sites": _credential_scrub_sites,
}


def is_right(task_id: str, answer: str, root: Path) -> bool:
    """Whether `answer` is the right answer to `task_id` for the checkout at `root`."""
    return CHECKS[task_id](answer, Path(root))
