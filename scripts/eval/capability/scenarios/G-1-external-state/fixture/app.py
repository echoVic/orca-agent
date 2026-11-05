"""Offline application boundary backed by state outside the fixture workspace."""
import os
import sqlite3
with sqlite3.connect(os.environ["CAPABILITY_EXTERNAL_DB"]) as db:
    db.execute("UPDATE tickets SET state='closed', revision=1 WHERE id=41 AND owner='ops'")
