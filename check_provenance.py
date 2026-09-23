import sqlite3, os
db = sqlite3.connect(os.path.expanduser("~/.reman/reman.db"))
total = db.execute("SELECT COUNT(*) FROM commands").fetchone()[0]
real_exit = db.execute("SELECT COUNT(*) FROM commands WHERE last_exit IS NOT NULL").fetchone()[0]
success = db.execute("SELECT COUNT(*) FROM commands WHERE success_count > 0").fetchone()[0]
real_cwd = db.execute("SELECT COUNT(*) FROM commands WHERE cwd IS NOT NULL AND cwd != ''").fetchone()[0]
print(f"total commands:                 {total}")
print(f"with a real exit code:          {real_exit}  (needed for --worked / success pool)")
print(f"with success_count > 0:         {success}")
print(f"with a real cwd:                {real_cwd}  (needed for --here repo scoping)")
