#!/usr/bin/env python3
"""
Reman Phase 7 - Actor attribution (human vs AI agent).

Tags each command run with who ran it: the human, or a named agent (agent:claude-code, ...).
WHY this needs its own path (R7): AI agents run shell commands in non-interactive subshells
where Atuin's hooks DON'T fire - Atuin generally never sees agent commands. So actor tracking
uses (a) an agent-provided hook (adapter #3, e.g. Claude Code PostToolUse -> reman_hook_claude.py)
and (b) env-marker detection where Reman can read the process env.

Non-goal unchanged: this does NOT loosen no-generation. Reman still only stores/returns real
executed commands; actor is just provenance on them.

DATA, NOT HARDCODED LOGIC: the per-agent marker map below is config-style data and markers move
fast - re-verify before relying (a lighter §4.0 check). The generic $AGENT convention check means
new convention-followers need no code change at all.
"""
import sqlite3, os, time, hashlib
from reman import DB_PATH, embed, pack

# Per-agent env markers for the HOLDOUTS that don't follow the emerging AGENT=<name> convention.
# Keep as data; re-verify before shipping reliance (markers change quickly).
AGENT_MARKERS = {
    "claude-code": ["CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT"],  # also CLAUDE_CODE_SESSION_ID
    "codex":       ["CODEX_CI"],                              # AGENT=codex request was declined
}


def detect_actor(env):
    """ActorDetector (T7.1). Two-step, highest-certainty first:
       1. $AGENT convention (Goose/Amp/Bun + any future follower) -> agent:<name>, generically.
       2. per-agent marker map for holdouts (Claude Code, Codex).
       else -> 'human': NOT an assertion of human, just 'not attributed to a known agent'
       (Cursor & sandboxed agents are a known gap - only a hook/own-capture can attribute them)."""
    a = (env.get("AGENT") or "").strip()
    if a:
        return f"agent:{a.lower()}"
    for name, markers in AGENT_MARKERS.items():
        if any(env.get(m) for m in markers):
            return f"agent:{name}"
    return "human"


def migrate_actor(db):
    cols = [r[1] for r in db.execute("PRAGMA table_info(commands)")]
    for c, ddl in [("human_runs", "INTEGER DEFAULT 0"),
                   ("agent_runs", "INTEGER DEFAULT 0"),
                   ("last_actor", "TEXT")]:
        if c not in cols:
            db.execute(f"ALTER TABLE commands ADD COLUMN {c} {ddl}")
    # optional full per-run audit trail (spec 2.3) - answers "what did agent X run here, when"
    db.execute("""CREATE TABLE IF NOT EXISTS executions (
                    id INTEGER PRIMARY KEY, command_id INTEGER, actor TEXT,
                    exit INTEGER, cwd TEXT, session TEXT, ts INTEGER)""")
    db.commit()


def record_run(db, cmd, exit_code, cwd, session, actor, ts=None, embed_fn=None):
    """Push-ingest one run, tagged with actor (T7.2). Upserts the command (embeds on first sight),
    bumps run/success/fail + human_runs|agent_runs, sets last_actor, appends an execution row.
    Captures exit/cwd even for agent subshells Atuin never saw."""
    ts = ts or int(time.time())
    h = hashlib.sha256(f"{cmd}\x00{cwd or ''}".encode()).hexdigest()
    # exit==0 -> success; exit>0 -> failure; exit<0 or None -> UNKNOWN (Atuin's PowerShell hook
    # records -1 when it can't capture the code) -> count as neither, so unknown != failed.
    ok = 1 if exit_code == 0 else 0
    bad = 1 if (exit_code is not None and exit_code > 0) else 0
    is_agent = actor.startswith("agent:")
    hinc, ainc = (0, 1) if is_agent else (1, 0)
    row = db.execute("""SELECT id, run_count, success_count, fail_count, human_runs, agent_runs
                        FROM commands WHERE cmd_hash=?""", (h,)).fetchone()
    if row:
        cid, rc, sc, fc, hr, ar = row
        db.execute("""UPDATE commands SET run_count=?, success_count=?, fail_count=?, last_exit=?,
                      last_used=?, human_runs=?, agent_runs=?, last_actor=? WHERE id=?""",
                   (rc + 1, sc + ok, fc + bad, exit_code, ts, hr + hinc, ar + ainc, actor, cid))
    else:
        cur = db.execute("""INSERT INTO commands
              (cmd_text, cmd_hash, cwd, first_seen, last_used, run_count, success_count, fail_count,
               last_exit, human_runs, agent_runs, last_actor)
              VALUES (?,?,?,?,?,1,?,?,?,?,?,?)""",
              (cmd, h, cwd, ts, ts, ok, bad, exit_code, hinc, ainc, actor))
        cid = cur.lastrowid
        fn = embed_fn or embed
        db.execute("INSERT OR REPLACE INTO command_vec (command_id, vec) VALUES (?,?)", (cid, pack(fn(cmd))))
    db.execute("""INSERT INTO executions (command_id, actor, exit, cwd, session, ts)
                  VALUES (?,?,?,?,?,?)""", (cid, actor, exit_code, cwd, session, ts))
    db.commit()
    return cid


def _matches(actor, flt):
    actor = actor or "human"
    if flt is None:
        return True
    if flt == "agent":
        return actor.startswith("agent:")
    if flt == "human":
        return not actor.startswith("agent:")     # human / unattributed bucket
    return actor == flt                            # exact, e.g. 'agent:claude-code'


def actor_log(db, actor_filter=None, n=20):
    """reman log --actor {human|agent|agent:<name>} (T7.3)."""
    out = []
    for cmd, actor, hr, ar, lu in db.execute(
            """SELECT cmd_text, last_actor, human_runs, agent_runs, last_used
               FROM commands WHERE last_actor IS NOT NULL ORDER BY last_used DESC"""):
        if _matches(actor, actor_filter):
            out.append({"command": cmd, "actor": actor or "human",
                        "human_runs": hr or 0, "agent_runs": ar or 0})
            if len(out) >= n:
                break
    return out


def actor_counts(db, cmd_texts):
    """{command: (human_runs, agent_runs, last_actor)} for MCP actor-weighting (T7.4)."""
    out = {}
    for c in cmd_texts:
        r = db.execute("SELECT human_runs, agent_runs, last_actor FROM commands WHERE cmd_text=? LIMIT 1",
                       (c,)).fetchone()
        out[c] = (r[0] or 0, r[1] or 0, r[2]) if r else (0, 0, None)
    return out


def provenance(db, cmd_texts):
    """Phase 2 inline provenance: {command: {success_rate, run_count, last_run, actor}} for
    showing trust signals next to each search result. Defensive if actor columns aren't present."""
    cols = [r[1] for r in db.execute("PRAGMA table_info(commands)")]
    has_actor = "last_actor" in cols
    sel = "success_count, run_count, last_used" + (", last_actor" if has_actor else "")
    out = {}
    for c in cmd_texts:
        r = db.execute(f"SELECT {sel} FROM commands WHERE cmd_text=? LIMIT 1", (c,)).fetchone()
        if not r:
            out[c] = {}; continue
        sc, rc, lu = r[0], r[1], r[2]
        actor = r[3] if has_actor and len(r) > 3 else None
        age = "?"
        if lu:
            d = (time.time() - lu) / 86400.0
            age = "today" if d < 1 else f"{int(d)}d ago"
        out[c] = {"success_rate": round((sc or 0) / (rc or 1), 2), "run_count": rc or 0,
                  "last_run": age, "actor": actor or "human"}
    return out


def main():
    import sys
    db = sqlite3.connect(DB_PATH)
    migrate_actor(db)
    if len(sys.argv) >= 2 and sys.argv[1] == "log":
        flt = None
        if "--actor" in sys.argv:
            flt = sys.argv[sys.argv.index("--actor") + 1]
        for r in actor_log(db, flt):
            print(f"  [{r['actor']:<18}] {r['command']}   (human={r['human_runs']} agent={r['agent_runs']})")
    else:
        print("usage: reman_actor.py log [--actor human|agent|agent:<name>]")


if __name__ == "__main__":
    main()
