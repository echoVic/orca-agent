"""Controlled context variants, not proof that the runtime compressed correctly."""
import os
from pathlib import Path
CONSTRAINTS = """Path: exports/weekly.csv
Limit: 137
Forbidden: network; delete archives
Unresolved: owner approval pending
Support: trial reduced latency by 8 ms
Opposition: trial lost 2 records
Decision: hold
"""
docs = Path("docs"); docs.mkdir(exist_ok=True)
variant = os.environ.get("CAPABILITY_CONTEXT_VARIANT", "full")
noise = "Historical batch notes are not approval evidence.\n" * 2000
text = noise[:len(noise)//2] + CONSTRAINTS + noise[len(noise)//2:] if variant == "full" else CONSTRAINTS
(docs / "CONTEXT.md").write_text(text)
print(f"{variant}: {len(text)} characters; supplied compacted input, no GUI/runtime compaction claim")
