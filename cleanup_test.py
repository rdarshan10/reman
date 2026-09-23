import sqlite3, os
db = sqlite3.connect(os.path.expanduser("~/.reman/reman.db"))
ids = [r[0] for r in db.execute("SELECT id, cmd_text FROM commands WHERE cwd IS NOT NULL AND cwd != ''")]
cmds = db.execute("SELECT cmd_text, cwd FROM commands WHERE cwd IS NOT NULL AND cwd != ''").fetchall()
for c, w in cmds:
    print(f"  removing: {c}  @ {w}")
for i in ids:
    db.execute("DELETE FROM command_vec WHERE command_id=?", (i,))
    db.execute("DELETE FROM command_desc_vec WHERE command_id=?", (i,))
    db.execute("DELETE FROM commands WHERE id=?", (i,))
db.commit()
print(f"removed {len(ids)} test commands + their vectors from reman.db")
