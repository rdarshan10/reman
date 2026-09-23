"""Phase 3 on REAL data: wire retention into the live reman.db (build repos from real cwds via
repo_identity, link commands), then DRY-RUN retention (no deletes) at real-now and at a simulated
future clock. Proves the retention integration on real data, safely."""
import sqlite3, os, time
from reman_enrich import repo_identity
from reman_retention import retention_pass

DB = os.path.expanduser("~/.reman/reman.db")
db = sqlite3.connect(DB)

# --- migrate: commands need repo_id + pinned; need a repos table (retention's expected schema)
cols = [r[1] for r in db.execute("PRAGMA table_info(commands)")]
if "repo_id" not in cols:
    db.execute("ALTER TABLE commands ADD COLUMN repo_id INTEGER")
if "pinned" not in cols:
    db.execute("ALTER TABLE commands ADD COLUMN pinned INTEGER DEFAULT 0")
db.execute("CREATE TABLE IF NOT EXISTS repos (id INTEGER PRIMARY KEY, identity TEXT UNIQUE, last_active INTEGER, ttl_days INTEGER)")
db.commit()

# --- populate repos from commands that have a real cwd, link repo_id, set last_active=max(last_used)
ident_id, last_active = {}, {}
for cid, cwd, lu in db.execute("SELECT id, cwd, last_used FROM commands WHERE cwd IS NOT NULL AND cwd!=''"):
    ident = repo_identity(cwd)
    if ident not in ident_id:
        cur = db.execute("INSERT OR IGNORE INTO repos (identity, last_active, ttl_days) VALUES (?,?,NULL)", (ident, lu or 0))
        rid = db.execute("SELECT id FROM repos WHERE identity=?", (ident,)).fetchone()[0]
        ident_id[ident] = rid
    rid = ident_id[ident]
    db.execute("UPDATE commands SET repo_id=? WHERE id=?", (rid, cid))
    last_active[rid] = max(last_active.get(rid, 0), lu or 0)
for rid, la in last_active.items():
    db.execute("UPDATE repos SET last_active=? WHERE id=?", (la, rid))
db.commit()

n_repos = db.execute("SELECT COUNT(*) FROM repos").fetchone()[0]
n_linked = db.execute("SELECT COUNT(*) FROM commands WHERE repo_id IS NOT NULL").fetchone()[0]
print(f"built {n_repos} repos from real cwds; linked {n_linked} commands")
print("repos:")
for ident, la in db.execute("SELECT identity, last_active FROM repos"):
    age = (time.time() - la) / 86400.0 if la else 9999
    print(f"  {ident[:55]:<55} last_active {age:.1f}d ago")

# --- DRY-RUN at real now (nothing abandoned yet -> expect no evictions)
dr, dc = retention_pass(db, now=time.time(), dry_run=True)
print(f"\nDRY-RUN @ now: would drop {len(dr)} repos, {len(dc)} stale commands  (all fresh -> expect 0)")

# --- DRY-RUN at now + 200 days (everything now 'abandoned' -> retention would clear unpinned)
future = time.time() + 200 * 86400
dr2, dc2 = retention_pass(db, now=future, dry_run=True)
print(f"DRY-RUN @ now+200d: would drop {len(dr2)} repos, {len(dc2)} commands  (proves the logic fires on real rows)")
print("nothing was actually deleted (dry_run=True throughout).")
