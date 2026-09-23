import sqlite3, os
db = sqlite3.connect(os.path.expanduser("~/.reman/reman.db"))
h = db.execute("SELECT COUNT(*) FROM help_cache").fetchone()[0]
d = db.execute("SELECT COUNT(*) FROM commands WHERE desc_source='rule'").fetchone()[0]
db.execute("DELETE FROM help_cache")
db.execute("UPDATE commands SET description=NULL, desc_source='none'")
db.execute("DELETE FROM command_desc_vec")
db.commit()
print(f"purged: {h} cached-help rows (incl. uninstall/notepad), reset {d} descriptions + their vectors")
print("command history + raw command vectors untouched")
