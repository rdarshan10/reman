import sqlite3, os
db = sqlite3.connect(os.path.expanduser("~/.reman/reman.db"))
db.execute("UPDATE commands SET description=NULL, desc_source='none'")
db.commit()
print("reset descriptions; ready to re-enrich")
