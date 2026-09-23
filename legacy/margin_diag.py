"""Diagnose a confidence signal: does TOP-vs-distribution separation distinguish
real matches from noise better than absolute sim? (generalization-relevant, seed-independent)"""
import sqlite3, os, numpy as np
from collections import defaultdict
from reman import DB_PATH, embed, unpack

db = sqlite3.connect(DB_PATH)
desc_vecs = defaultdict(list)
for cid, vec in db.execute("SELECT command_id, vec FROM command_desc_vec"):
    desc_vecs[cid].append(np.asarray(unpack(vec)))
rows = []
for cid, cmd, rawv in db.execute("SELECT c.id,c.cmd_text,cv.vec FROM commands c JOIN command_vec cv ON cv.command_id=c.id"):
    if cmd.strip().startswith("#"):
        continue
    rows.append((cid, np.asarray(unpack(rawv))))

def sims_for(q):
    qv = np.asarray(embed(q)); qn = np.linalg.norm(qv)
    out = []
    for cid, rv in rows:
        sr = float(qv @ rv / (qn * np.linalg.norm(rv)))
        sd = max((float(qv @ d / (qn * np.linalg.norm(d))) for d in desc_vecs.get(cid, [])), default=-1)
        out.append(max(sr, sd))
    return np.array(out)

REAL = ["run database migrations", "copy the app folder to the server over ssh",
        "tear down containers and wipe volumes", "start expo with a clean cache",
        "install python dependencies", "list docker containers"]
NOISE = ["zzqq blarghustle frobnicate", "that ffmpeg thing that downscaled video",
         "asdfghjkl qwerty uiop", "the purple elephant danced quietly"]

print(f"{'query':<46} {'top':>6} {'mean':>6} {'p95':>6} {'std':>6} {'z':>6} {'top-p95':>7}")
def show(q, tag):
    s = sims_for(q)
    top, mean, p95, std = s.max(), s.mean(), np.percentile(s, 95), s.std()
    z = (top - mean) / std
    print(f"[{tag}] {q[:40]:<40} {top:6.3f} {mean:6.3f} {p95:6.3f} {std:6.3f} {z:6.2f} {top-p95:7.3f}")
for q in REAL:  show(q, "R")
print()
for q in NOISE: show(q, "N")
