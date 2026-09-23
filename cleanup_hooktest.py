import sqlite3, os
db = sqlite3.connect(os.path.expanduser("~/.reman/reman.db"))
synthetic = [
    'echo "reman-hook-live-proof-9animal-marker"',
    'docker compose logs -f api',
    'pytest -q tests/unit',
]
removed = 0
for c in synthetic:
    cur = db.execute("SELECT id FROM commands WHERE cmd_text=?", (c,))
    for (cid,) in cur.fetchall():
        db.execute("DELETE FROM command_vec WHERE command_id=?", (cid,))
        db.execute("DELETE FROM command_desc_vec WHERE command_id=?", (cid,))
        db.execute("DELETE FROM executions WHERE command_id=?", (cid,))
        db.execute("DELETE FROM commands WHERE id=?", (cid,))
        removed += 1
db.commit()
print(f"removed {removed} synthetic hook-test commands from reman.db")
