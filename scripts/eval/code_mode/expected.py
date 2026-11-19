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
# A list index at the start of a line: "3. ", "- 3) ", "| 3. ".
LIST_INDEX = re.compile(r"^\s*\|?\s*(?:[-*\u2022]\s*)?\d+[.)]\s+")
BULLET = re.compile(r"^\s*\|?\s*[-*\u2022]\s+")
WORKFLOW_NAME = re.compile(r"workflow\.[a-z_.]*[a-z_]")
# What may come before the first word of an item: spaces, a table pipe, a
# bullet, a list index, backticks and bold markers.
DECORATION = re.compile(r"^[\s|`*]*(?:[-*\u2022]\s+)?(?:\d+[.)]\s+)?[\s`*]*")
EVENT_TYPE = re.compile(r'rename\s*=\s*"(workflow\.[a-z_.]+)"')


def _read(path: Path) -> str:
    return path.read_text(encoding="utf-8", errors="replace")


def _numbers(text: str) -> set[str]:
    return set(NUMBER.findall(text))


def _without_list_index(line: str) -> str:
    return LIST_INDEX.sub("", line, count=1)


def _names(path: str, mentioned: str) -> bool:
    """Whether `mentioned`, a path as an answer wrote it, is `path`."""
    return path == mentioned or path.endswith("/" + mentioned)


def _only_these_files(answer: str, allowed: list[str]) -> bool:
    """Every file the answer lists is one of `allowed`.

    Only a line that lists a file names it: one that starts, after its bullet,
    index, table pipe and markers, with a Rust path. A path mentioned later in
    a line, or in a sentence, names nothing.
    """
    for line in answer.splitlines():
        found = RUST_PATH.match(DECORATION.sub("", line, count=1))
        if found and not any(_names(path, found.group(0)) for path in allowed):
            return False
    return True


def _each_with_its_count(answer: str, counts: dict[str, int], under: str = "") -> bool:
    """Every path is on a line of the answer that also gives its count."""
    for path, count in counts.items():
        short = path.removeprefix(under)
        if not any(short in line and str(count) in _numbers(line.replace(short, " "))
                   for line in map(_without_list_index, answer.splitlines())):
            return False
    return True


def _user_trusted_launches(answer: str, root: Path) -> bool:
    counts = {}
    for path in sorted((root / RUNTIME_SRC).rglob("*.rs")):
        count = _read(path).count("launch_user_trusted(")
        if count:
            counts[path.relative_to(root).as_posix()] = count
    if not counts:
        raise RuntimeError(f"no launch_user_trusted( calls found under {root / RUNTIME_SRC}")
    return (_each_with_its_count(answer, counts, under=RUNTIME_SRC)
            and _only_these_files(answer, list(counts)))


def _workflow_events(answer: str, root: Path) -> bool:
    source = _read(root / "crates/orca-core/src/event_schema.rs")
    events = set(EVENT_TYPE.findall(source))
    if not events:
        raise RuntimeError("no workflow event types found in event_schema.rs")
    named: set[str] = set()
    stated: set[str] = set()
    total_lines: set[str] = set()
    for line in answer.splitlines():
        listed = _events_listed(line)
        if listed:
            named |= listed
            continue
        numbers = _numbers(_without_list_index(line))
        stated |= numbers
        if "total" in line.lower():
            total_lines |= numbers
    total = str(len(events))
    return named == events and total in stated and total_lines <= {total}


def _events_listed(line: str) -> set[str]:
    """The events a line lists: one made of nothing but event names."""
    text = DECORATION.sub("", line, count=1).replace("`", "").replace("*", "")
    text = text.strip().rstrip(".,;:").strip()
    names = [name for name in re.split(r"[,\s]+", text) if name]
    if names and all(WORKFLOW_NAME.fullmatch(name) for name in names):
        return set(names)
    return set()


def _tools_named(answer: str, every: set[str]) -> set[str]:
    """Tools the answer names: inside backticks, or on a line of nothing but names."""
    named: set[str] = set()
    for span in re.findall(r"`([^`]*)`", answer):
        named |= set(re.findall(r"[a-z_]+", span)) & every
    for line in answer.splitlines():
        line = BULLET.sub("", _without_list_index(line), count=1).strip().strip("|")
        words = [word.strip("`").rstrip(".;:`") for word in re.split(r"[\s,]+", line)]
        words = [word for word in words if word and word != "and"]
        if words and all(word in every for word in words):
            named |= set(words)
    return named


def _network_tools(answer: str, root: Path) -> bool:
    source = _read(root / "crates/orca-tools/src/registry.rs")
    every = set(re.findall(r'_spec\(\s*"([a-z_]+)",', source))
    network = set(re.findall(
        r'_spec\(\s*"([a-z_]+)",(?:(?!_spec\().)*?CapabilitySet::network_search\(\)',
        source, re.S))
    if not network:
        raise RuntimeError("no network tools found in registry.rs")
    return _tools_named(answer, every) == network


def _largest_test_files(answer: str, root: Path) -> bool:
    counts = {path.relative_to(root).as_posix(): _read(path).count("#[test]")
              for path in (root / "tests").glob("*.rs")}
    if not counts:
        raise RuntimeError(f"no Rust files under {root / 'tests'}")
    largest = sorted(counts, key=lambda path: (-counts[path], path))[:5]
    if not _each_with_its_count(answer, {path: counts[path] for path in largest}):
        return False
    if not _only_these_files(answer, largest):
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
    if not calling:
        raise RuntimeError("no calls of remove_credential_env( found")
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
