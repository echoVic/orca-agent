import os
import sqlite3
with sqlite3.connect(os.environ["CAPABILITY_EXTERNAL_DB"]) as db:
    db.execute("CREATE TABLE tickets(id INTEGER PRIMARY KEY, owner TEXT, state TEXT, revision INTEGER)")
    db.execute("INSERT INTO tickets VALUES(41, 'ops', 'open', 0)")
