import sqlite3, os
db = sqlite3.connect(os.path.expanduser("~/.reman/reman.db"))
r = db.execute("SELECT cmd_text, last_actor, last_exit, cwd FROM commands WHERE cmd_text LIKE '%9animal-marker%'").fetchall()
if r:
    print("CAPTURED via live hook:")
    for row in r:
        print("  ", row)
else:
    print("NOT captured — hook not active in THIS session (mid-session settings edit).")
    print("New sessions will pick it up; or open /hooks once to reload config now.")
