#!/usr/bin/env python3
"""L4 preparation: add a large architecture document to the L3 crate fixture.

The scenario's difficulty is context scale, not code size: the crate is unchanged, but the workspace
now carries ~50 k tokens of architecture notes, and the money-rounding policy is one paragraph inside
that document. The agent has to find the paragraph instead of being pointed at it.

Deterministic on purpose: same bytes for every attempt so runs are comparable.
"""

from __future__ import annotations

import sys
from pathlib import Path

SECTION_COUNT = 120
PARAGRAPH = (
    "The pipeline keeps every monetary value in integer cents and never stores floats. Modules own a "
    "single step and communicate through plain data structures; rendering happens only in `receipt` "
    "and `report`. New behaviour belongs in the module that already owns the concern, and public "
    "re-exports stay in `lib.rs`."
)
POLICY = (
    "Money rounding: all monetary values use round-half-to-even (banker's rounding). A value exactly "
    "halfway between two cents goes to the even cent, so 0.5 -> 0, 1.5 -> 2, 2.5 -> 2. The legacy "
    "`round_half_up` helper is deprecated and must not be called by new code."
)


def main() -> int:
    workspace = Path.cwd()
    docs = workspace / "docs"
    docs.mkdir(parents=True, exist_ok=True)
    lines = [
        "# Architecture notes",
        "",
        "This document records how the order-pricing pipeline is organised. It is descriptive, not a "
        "specification of the public API; the tests remain authoritative for behaviour.",
        "",
    ]
    for index in range(SECTION_COUNT):
        lines += [
            f"## Section {index + 1}: pipeline stage {index + 1}",
            "",
            PARAGRAPH,
            "",
            "Operational notes:",
            "",
            f"- Stage {index + 1} consumes the output of stage {index} without mutating it.",
            f"- Failures at stage {index + 1} are reported to the caller, never swallowed.",
            "- Cents are integers; formatting is a presentation concern.",
            "",
        ]
        if index == 46:
            lines += ["## Money and rounding", "", POLICY, ""]
    lines += [
        "## Deprecated helpers",
        "",
        "`round_half_up` predates the policy section above. It stays in the tree for compatibility of "
        "historical branches, but new code must follow the money-and-rounding rule.",
        "",
    ]
    (docs / "ARCHITECTURE.md").write_text("\n".join(lines), encoding="utf-8")
    text = (docs / "ARCHITECTURE.md").read_text(encoding="utf-8")
    print(f"wrote docs/ARCHITECTURE.md ({len(text)} chars, ~{len(text) // 4} tokens)", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
