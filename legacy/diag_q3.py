import sqlite3, os, numpy as np
from reman import DB_PATH, embed, unpack

intent = "tear down containers and wipe volumes"
qv = np.asarray(embed(intent))
def cos(a,b):
    a,b=np.asarray(a),np.asarray(b)
    return float(a@b/(np.linalg.norm(a)*np.linalg.norm(b))) if a.any() and b.any() else 0.0

db = sqlite3.connect(DB_PATH)
rows = db.execute("""SELECT c.cmd_text, c.description, c.run_count, cv.vec, dv.vec
                     FROM commands c JOIN command_vec cv ON cv.command_id=c.id
                     LEFT JOIN command_desc_vec dv ON dv.command_id=c.id
                     WHERE c.cmd_text LIKE '%docker-compose down%' OR c.cmd_text LIKE '%docker compose down%'""").fetchall()
print(f"INTENT: {intent}\n")
for cmd, desc, rc, rawv, descv in rows:
    sr = cos(qv, unpack(rawv))
    sd = cos(qv, unpack(descv)) if descv is not None else None
    print(f"CMD : {cmd}  (runs={rc})")
    print(f"DESC: {desc}")
    print(f"sim_raw={sr:.3f}  sim_desc={sd if sd is None else round(sd,3)}\n")
