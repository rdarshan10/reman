import sqlite3, os
db = sqlite3.connect(os.path.expanduser("~/.reman/reman.db"))
for (cid,) in db.execute("SELECT id FROM commands WHERE cmd_text='kubectl get pods --namespace prod'").fetchall():
    db.execute("DELETE FROM command_vec WHERE command_id=?", (cid,))
    db.execute("DELETE FROM command_desc_vec WHERE command_id=?", (cid,))
    db.execute("DELETE FROM executions WHERE command_id=?", (cid,))
    db.execute("DELETE FROM commands WHERE id=?", (cid,))
db.commit()
print("removed kubectl test command")
