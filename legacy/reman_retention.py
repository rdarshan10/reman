#!/usr/bin/env python3
"""
Reman Phase 3 - Hybrid retention (auto-cleanup). Background job, NEVER on the query path.

Two passes, both honour pinning + high run_count:
  - per-repo:    if now - repo.last_active > repo_ttl  -> drop the repo and ALL its commands
                 (a dead project vanishes as a block), UNLESS it holds a pinned/high-run command.
  - per-command: within still-active repos, if now - cmd.last_used > cmd_ttl -> drop that command
                 (a one-off), unless pinned or run_count >= PIN_THRESHOLD.
Defaults: repo_ttl=90d, cmd_ttl=30d (repo_ttl > cmd_ttl); never evict pinned or run_count>=20.

This module is SAFE: retention_pass operates on whatever db connection you pass it. The
`simulate` AC builds a throwaway in-memory DB - it never touches your real reman.db.
Running retention on real data is opt-in and gated (see main()).
"""
import sqlite3, sys, os, time

DAY = 86400
DEFAULT_REPO_TTL = 90
DEFAULT_CMD_TTL  = 30
PIN_THRESHOLD    = 20


def retention_pass(db, now, repo_ttl_days=DEFAULT_REPO_TTL, cmd_ttl_days=DEFAULT_CMD_TTL,
                   pin_threshold=PIN_THRESHOLD, dry_run=False):
    cmd_ttl = cmd_ttl_days * DAY
    dropped_repos, dropped_cmds = [], []

    # --- per-repo: abandoned project -> drop as a block, unless it protects a pinned, high-run,
    #     OR still-FRESH command. The freshness clause is defensive: it does NOT trust
    #     repo.last_active (an invariant maintained by touch-on-use in another module). If that
    #     invariant ever drifts, a fresh command still protects its repo from block-deletion.
    for rid, identity, last_active, ttl_days in db.execute(
            "SELECT id, identity, last_active, ttl_days FROM repos").fetchall():
        this_repo_ttl = (ttl_days if ttl_days is not None else repo_ttl_days) * DAY  # per-repo override (spec 2.1)
        if now - last_active <= this_repo_ttl:
            continue
        protected = db.execute(
            "SELECT COUNT(*) FROM commands WHERE repo_id=? AND "
            "(pinned=1 OR run_count>=? OR last_used > ?)",
            (rid, pin_threshold, now - cmd_ttl)).fetchone()[0]
        if protected:
            continue
        cmds = [r[0] for r in db.execute("SELECT cmd_text FROM commands WHERE repo_id=?", (rid,))]
        dropped_repos.append((identity, cmds))
        if not dry_run:
            db.execute("DELETE FROM commands WHERE repo_id=?", (rid,))
            db.execute("DELETE FROM repos WHERE id=?", (rid,))

    # --- per-command: stale one-offs in surviving repos
    surviving = {r[0] for r in db.execute("SELECT id FROM repos").fetchall()}
    for cid, cmd, last_used, rc, pinned, rid in db.execute(
            "SELECT id, cmd_text, last_used, run_count, pinned, repo_id FROM commands").fetchall():
        if rid not in surviving:
            continue
        if now - last_used > cmd_ttl and not pinned and rc < pin_threshold:
            dropped_cmds.append(cmd)
            if not dry_run:
                db.execute("DELETE FROM commands WHERE id=?", (cid,))
    if not dry_run:
        db.commit()
    return dropped_repos, dropped_cmds


SIM_SCHEMA = """
CREATE TABLE repos (id INTEGER PRIMARY KEY, identity TEXT, last_active INTEGER, ttl_days INTEGER);
CREATE TABLE commands (id INTEGER PRIMARY KEY, cmd_text TEXT, repo_id INTEGER,
                       last_used INTEGER, run_count INTEGER DEFAULT 1, pinned INTEGER DEFAULT 0);
"""


def simulate():
    now = 1_700_000_000
    db = sqlite3.connect(":memory:")
    db.executescript(SIM_SCHEMA)
    R = "INSERT INTO repos(id,identity,last_active,ttl_days) VALUES (?,?,?,?)"
    C = "INSERT INTO commands(cmd_text,repo_id,last_used,run_count,pinned) VALUES (?,?,?,?,?)"

    # Repo A: abandoned 120d, only ordinary commands -> vanishes as a block
    db.execute(R, (1, 'repoA (abandoned)', now - 120 * DAY, None))
    db.execute(C, ('A: npm build', 1, now - 120 * DAY, 5, 0))
    db.execute(C, ('A: npm test', 1, now - 121 * DAY, 3, 0))
    # Repo B: active 3d -> survives; mixed commands
    db.execute(R, (2, 'repoB (active)', now - 3 * DAY, None))
    db.execute(C, ('B: rerun recently', 2, now - 2 * DAY, 10, 0))   # keep (fresh)
    db.execute(C, ('B: stale one-off', 2, now - 40 * DAY, 1, 0))    # DROP (40d>30d, run1)
    db.execute(C, ('B: pinned old', 2, now - 200 * DAY, 2, 1))      # keep (pinned)
    db.execute(C, ('B: high-run old', 2, now - 50 * DAY, 25, 0))    # keep (run>=20)
    # Repo C: abandoned 200d but holds a pinned cmd -> repo survives; its stale one-off still drops
    db.execute(R, (3, 'repoC (abandoned but pinned)', now - 200 * DAY, None))
    db.execute(C, ('C: pinned deploy', 3, now - 200 * DAY, 4, 1))   # keep (protects repo)
    db.execute(C, ('C: stale one-off', 3, now - 60 * DAY, 1, 0))    # DROP via per-command pass (note 2)
    # Repo D: last_active DRIFTED stale (120d) but holds a FRESH command -> the bug the AC missed.
    #         freshness must protect the repo even though last_active says "abandoned".
    db.execute(R, (4, 'repoD (drift: stale last_active, fresh cmd)', now - 120 * DAY, None))
    db.execute(C, ('D: actively used', 4, now - 1 * DAY, 2, 0))     # keep (fresh -> protects repo)
    # Repo E: per-repo ttl override = 10d. last_active 40d -> abandoned by ITS ttl (40>10) though
    #         the 90d default would have KEPT it; its cmd is also stale (40>30) so freshness can't
    #         protect -> repoE block-deletes. Cleanly isolates the per-repo ttl override (note 1).
    db.execute(R, (5, 'repoE (short ttl=10d)', now - 40 * DAY, 10))
    db.execute(C, ('E: ordinary', 5, now - 40 * DAY, 1, 0))        # block-deleted (40d > 10d override)
    db.commit()

    def surviving():
        return {r[0] for r in db.execute("SELECT cmd_text FROM commands").fetchall()}
    def repos_left():
        return {r[0] for r in db.execute("SELECT identity FROM repos").fetchall()}

    before = surviving()
    dropped_repos, dropped_cmds = retention_pass(db, now)
    after = surviving()

    expect_gone = {'A: npm build', 'A: npm test', 'B: stale one-off', 'C: stale one-off', 'E: ordinary'}
    expect_kept = {'B: rerun recently', 'B: pinned old', 'B: high-run old',
                   'C: pinned deploy', 'D: actively used'}
    gone = before - after
    repo_ok = repos_left() == {'repoB (active)', 'repoC (abandoned but pinned)',
                               'repoD (drift: stale last_active, fresh cmd)'}
    ok = (gone == expect_gone) and expect_kept.issubset(after) and repo_ok

    print("dropped repos (as a block):")
    for ident, cmds in dropped_repos:
        print(f"  - {ident}: {cmds}")
    print(f"dropped stale one-offs: {dropped_cmds}")
    print(f"\nsurviving repos: {sorted(repos_left())}")
    print(f"survivors:       {sorted(after)}")
    print(f"\nexpected gone: {sorted(expect_gone)}")
    print(f"actually gone: {sorted(gone)}")
    print(f"\nAC {'PASS' if ok else 'FAIL'}:")
    print("  - abandoned repoA vanished as a block; active repoB kept re-run/pinned/high-run, dropped one-off")
    print("  - repoC (abandoned+pinned) SURVIVED but its stale one-off was dropped (per-cmd pass on survivor)")
    print("  - repoD (stale last_active but FRESH cmd) SURVIVED -> freshness defends against last_active drift")
    print("  - repoE dropped at its per-repo ttl_days=10 override (not the 90d default)")
    return ok


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "simulate":
        sys.exit(0 if simulate() else 1)
    else:
        print("usage: reman_retention.py simulate    (runs the Phase 3 clock-simulation AC)")
