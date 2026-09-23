import sqlite3, os
db = sqlite3.connect(os.path.expanduser("~/.reman/reman.db"))
def n(sql, *a): return db.execute(sql, a).fetchone()[0]
print("git status present:", n("SELECT COUNT(*) FROM commands WHERE cmd_text='git status'"))
print("docker ps present: ", n("SELECT COUNT(*) FROM commands WHERE cmd_text='docker ps'"))
print("\nsample 'git ...' commands actually in seed:")
for r in db.execute("SELECT cmd_text FROM commands WHERE cmd_text LIKE 'git %' LIMIT 10"):
    print("  ", r[0])
