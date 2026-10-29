#!/usr/bin/env python3
"""L5-grade context preparation for axis C: a large spec with one needle.

The scenario's difficulty is the amount of text the agent must search, not the task: a single
`Constraint-<id>` line names the exact value to write into `answer.txt`, and it sits at a deterministic
position inside a document of roughly the requested token budget.
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

PARAGRAPH = (
    "The pipeline processes orders in batches. Each batch keeps its own cursor, and a batch never "
    "observes the cursor of another batch. Retries replay from the last acknowledged cursor, which is "
    "persisted before the next batch starts. Operators can pause a batch without losing its cursor."
)


def main() -> int:
    target_tokens = int(os.environ.get("CAPABILITY_CONTEXT_TOKENS", "50000"))
    workspace = Path.cwd()
    docs = workspace / "docs"
    docs.mkdir(parents=True, exist_ok=True)
    needle_line = (
        "Constraint-{id}: the report must use the SKU {sku} verbatim, in upper case, as its only value."
    )
    lines: list[str] = ["# Batch processing spec", ""]
    approx_chars = 0
    section = 0
    while approx_chars < target_tokens * 4:
        section += 1
        lines += [f"## Section {section}", "", PARAGRAPH, ""]
        approx_chars += len(PARAGRAPH) + 40
        if section == 87:
            lines += ["## Reporting constraint", "", needle_line.format(id="C-87", sku="ZX-9100"), ""]
    lines += ["## Reporting constraint (duplicate marker)", "", needle_line.format(id="C-87", sku="ZX-9100"), ""]
    (docs / "SPEC.md").write_text("\n".join(lines), encoding="utf-8")
    print(f"wrote docs/SPEC.md ~{target_tokens} tokens", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
