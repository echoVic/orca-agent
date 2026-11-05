"""Query independently persisted state; do not read app.py or completion text."""
import os
from pathlib import Path
import sqlite3
path = Path(os.environ["CAPABILITY_EXTERNAL_DB"])
if not path.is_file(): raise SystemExit(1)
with sqlite3.connect(f"file:{path}?mode=ro", uri=True) as db:
    actual = db.execute("SELECT id, owner, state, revision FROM tickets ORDER BY id").fetchall()
raise SystemExit(0 if actual == [(41, 'ops', 'closed', 1)] else 1)
