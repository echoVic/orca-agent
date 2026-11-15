#!/usr/bin/env python3
"""B-L4-real preparation: copy the real Orca repository into the workspace.

Only sources are copied (no target/, jobs/, .git/, .superpowers/), and the hidden test is written
next to the workspace (the attempt directory, mounted at /artifacts) so the agent never sees it.
"""

from __future__ import annotations

import os
import shutil
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[4]
SKIP = {"target", "jobs", ".git", ".superpowers", "node_modules", ".worktrees", "site/node_modules"}

MULTI_TEST = '''use orca_file_search::{normalize_query_path, normalize_query_paths};

#[test]
fn normalizes_single_paths() {
    assert_eq!(normalize_query_path("a/./b//c/"), "a/b/c");
    assert_eq!(normalize_query_path("a/b/../c"), "a/c");
}

#[test]
fn normalizes_lists_and_drops_empty_results() {
    let out = normalize_query_paths(&[
        "a//b/".to_string(),
        "".to_string(),
        "a/b".to_string(),
        "x/../y".to_string(),
    ]);
    assert_eq!(out, vec!["a/b".to_string(), "y".to_string()]);
}

#[test]
fn keeps_order_and_deduplicates() {
    let out = normalize_query_paths(&["a".to_string(), "a/".to_string(), "./a".to_string(), "b".to_string()]);
    assert_eq!(out, vec!["a".to_string(), "b".to_string()]);
}
'''

HIDDEN_TEST = '''use orca_file_search::normalize_query_path;

#[test]
fn collapses_dot_and_empty_segments() {
    assert_eq!(normalize_query_path("a/./b//c/"), "a/b/c");
}

#[test]
fn keeps_the_meaning_of_dotdot() {
    assert_eq!(normalize_query_path("a/b/../c"), "a/c");
}

#[test]
fn empty_input_is_empty() {
    assert_eq!(normalize_query_path(""), "");
}
'''


def main() -> int:
    workspace = Path.cwd()
    for entry in workspace.iterdir():
        if entry.name in {".git", "..", "."}:
            continue
        if entry.is_dir():
            shutil.rmtree(entry, ignore_errors=True)
        else:
            entry.unlink(missing_ok=True)
    for child in REPO.iterdir():
        if child.name in SKIP:
            continue
        target = workspace / child.name
        if child.is_dir():
            shutil.copytree(child, target, symlinks=True,
                            ignore=shutil.ignore_patterns(*SKIP))
        else:
            shutil.copy2(child, target)
    variant = os.environ.get("CAPABILITY_REAL_TASK", "single")
    if variant == "multi":
        hidden = Path("..") / "hidden_real_multi.rs"
        hidden.write_text(MULTI_TEST, encoding="utf-8")
    else:
        hidden = Path("..") / "hidden_real.rs"
        hidden.write_text(HIDDEN_TEST, encoding="utf-8")
    print(f"copied real repo; hidden test at {hidden}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
