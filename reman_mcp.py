#!/usr/bin/env python3
"""
Reman Phase 6 - MCP agent interface (the moat).

Exposes the curated command store to an AI agent as GROUND-TRUTH retrieval, so the agent
reuses the user's real successful commands instead of guessing.

HARD CONTRACT (spec 0 non-goal #1 + spec 4.6): every result is a REAL command the user
actually ran, provenance-stamped and traceable. These tools NEVER return a generated/guessed
command. The agent is told: prefer reman.search results over generating.

Run as an MCP stdio server:   python reman_mcp.py
Or import the reman_* functions directly (they are plain functions; the MCP wrapper is thin).
"""
import sqlite3, os, time, re
from reman import DB_PATH
from reman_enrich import search as _search, did_you_mean as _dym


# ---- SECRET REDACTION --------------------------------------------------------------------------
# Commands inside the allowed root can still embed secrets. Scrub obvious ones before handing a
# command to an agent, keeping the command's SHAPE (so it's still useful) but hiding the value.
# Conservative patterns - aimed at real leaks, tuned to avoid mangling normal commands.
_SECRET_PATTERNS = [
    (re.compile(r'(?i)\b(password|passwd|pwd|secret|token|api[_-]?key|apikey|access[_-]?key|'
                r'auth[_-]?token|client[_-]?secret)(\s*[=:]\s*)(\S+)'), r'\1\2***'),
    (re.compile(r'(?i)(--password[=\s]+|--token[=\s]+)(\S+)'), r'\1***'),
    (re.compile(r'(?i)(authorization:\s*(?:bearer|basic)\s+)(\S+)'), r'\1***'),
    (re.compile(r'(://[^:@/\s]+:)([^@/\s]+)(@)'), r'\1***\3'),                 # user:pass@host
    (re.compile(r'\bAKIA[0-9A-Z]{16}\b'), '***'),                             # AWS access key id
    (re.compile(r'(?i)\b(?:ghp|gho|ghs|ghu|github_pat)_[A-Za-z0-9_]{20,}\b'), '***'),  # GitHub tokens
    (re.compile(r'\bxox[baprs]-[A-Za-z0-9-]{10,}\b'), '***'),                 # Slack tokens
    (re.compile(r'\bsk-[A-Za-z0-9]{20,}\b'), '***'),                          # OpenAI-style keys
]


def _redact(text):
    """Hide obvious secrets in a command string before returning it to an agent."""
    if not text:
        return text
    for rx, repl in _SECRET_PATTERNS:
        text = rx.sub(repl, text)
    return text


# Strict mode (REMAN_MCP_STRICT_SECRETS=1): if a command STILL looks secret-bearing after redaction,
# drop the whole command rather than risk a partial leak. High-confidence residual signals only, so
# ordinary things (git SHAs / sha256 digests = pure-hex, UUIDs) are NOT dropped.
_MCP_STRICT = os.environ.get("REMAN_MCP_STRICT_SECRETS") == "1"
_RESIDUAL_SECRET = [
    re.compile(r'eyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{4,}'),     # JWT
    re.compile(r'(?i)-----BEGIN[ A-Z]*PRIVATE KEY'),                                # PEM private key
    # opaque high-entropy token: 40+ chars mixing lower+upper+digit (spares pure-hex SHAs/digests)
    re.compile(r'(?=[A-Za-z0-9+/=_-]{40,})(?=[^ ]*[a-z])(?=[^ ]*[A-Z])(?=[^ ]*[0-9])[A-Za-z0-9+/=_-]{40,}'),
]


def _residual_secret(text):
    return bool(text) and any(rx.search(text) for rx in _RESIDUAL_SECRET)


def _safe_command(cmd):
    """Redact secrets; in strict mode, WITHHOLD entirely if one still seems present. Returns the
    safe command string, or None if it should not be shown to the agent."""
    red = _redact(cmd)
    if _MCP_STRICT and _residual_secret(red):
        return None
    return red


def _folder_match(cwd_value, folder):
    """True if a command's stored cwd is the SAME directory as `folder` (normalised, case-insensitive
    on Windows). Agents scope by exact folder - the same unit the user's picker uses - not by repo."""
    if not cwd_value or not folder:
        return False
    return os.path.normcase(os.path.normpath(cwd_value)) == os.path.normcase(os.path.normpath(folder))


# ---- SECURITY BOUNDARY -------------------------------------------------------------------------
# An MCP agent must NOT be able to read command history from outside its workspace - other projects'
# commands routinely contain secrets, hosts, and tokens. The boundary is set from SERVER config
# (env), NOT from the cwd argument (an agent can pass any cwd, so it is untrusted). Every tool only
# ever returns commands whose stored cwd is inside the allowed root.
#   REMAN_MCP_ROOT=<dir>        allowed root (defaults to the dir the server was launched in)
#   REMAN_MCP_ALLOW_GLOBAL=1    opt out of the boundary entirely (personal/trusted use only)
_MCP_ALLOW_GLOBAL = os.environ.get("REMAN_MCP_ALLOW_GLOBAL") == "1"
# REMAN_MCP_ROOT may list several allowed roots (os.pathsep-separated: ';' on Windows, ':' on POSIX).
_MCP_ROOTS = [os.path.normcase(os.path.normpath(p)) for p in
              (os.environ.get("REMAN_MCP_ROOT") or os.getcwd()).split(os.pathsep) if p.strip()]


def _within_root(path):
    """True if `path` is one of the allowed roots or a folder beneath one. The hard security gate."""
    if _MCP_ALLOW_GLOBAL:
        return True
    if not path:
        return False
    p = os.path.normcase(os.path.normpath(path))
    return any(p == r or p.startswith(r + os.sep) for r in _MCP_ROOTS)


def _cmd_within_root(db, cmd):
    """True if this command was ever run inside the allowed root (for results that carry no cwd)."""
    if _MCP_ALLOW_GLOBAL:
        return True
    return any(_within_root(c) for (c,) in db.execute("SELECT cwd FROM commands WHERE cmd_text=?", (cmd,)))


def reman_search(intent: str, cwd: str = None, worked_only: bool = True, k: int = 5,
                 prefer: str = None) -> list:
    """Retrieve the user's REAL past commands by meaning. worked_only restricts to the success
    pool (commands seen to exit 0). cwd scopes to commands run in that exact folder (the same
    folder-level unit the user's picker uses). prefer='human' weights human-verified successes
    above agent-run equivalents (a human-verified command is more trustworthy than one an agent
    generated-then-happened-to-exit-0). Returns real commands only - never generated."""
    if cwd is not None and not _within_root(cwd):
        return []                                     # security: requested folder outside allowed root
    pool = 200 if (cwd or not _MCP_ALLOW_GLOBAL) else (k * 3 if prefer else k)
    mode, items = _search(intent, k=pool, worked_only=worked_only, here=None)
    db = sqlite3.connect(DB_PATH)
    out = []
    if mode == "manual":
        for n, rc, cmd in items:
            if not _cmd_within_root(db, cmd):
                continue
            safe = _safe_command(cmd)
            if safe is None:
                continue
            out.append({"command": safe, "run_count": rc, "match": "literal", "generated": False})
        return out[:k]
    for it, gk, size in items:
        _score, sim, _sr, _sd, _won, cmd, desc, rc, c = it
        if not _within_root(c):                        # security boundary
            continue
        if cwd is not None and not _folder_match(c, cwd):
            continue
        safe = _safe_command(cmd)
        if safe is None:
            continue
        out.append({"command": safe, "cwd": c, "run_count": rc, "intent": gk,
                    "variants": size, "similarity": round(sim, 3),
                    "description": desc, "generated": False})
    from reman_actor import provenance                       # Phase 2 inline provenance
    pr = provenance(sqlite3.connect(DB_PATH), [r["command"] for r in out])
    for r in out:
        p = pr.get(r["command"], {})
        r["success_rate"] = p.get("success_rate")
        r["last_run"] = p.get("last_run")
        r["actor"] = p.get("actor", "human")
    if prefer == "human":
        from reman_actor import actor_counts
        counts = actor_counts(sqlite3.connect(DB_PATH), [r["command"] for r in out])
        for r in out:
            hr, ar, actor = counts.get(r["command"], (0, 0, None))
            r["actor"] = actor or "human"
            r["human_runs"], r["agent_runs"] = hr, ar
        # stable re-rank: human-verified (human_runs>0) first, preserving semantic order within
        out.sort(key=lambda r: 0 if r.get("human_runs", 0) > 0 else 1)
    return out[:k]


def reman_recent(cwd: str = None, n: int = 20) -> list:
    """Chronological recent REAL commands (the agent's short-term memory). cwd scopes to that exact
    folder (the same folder-level unit the user's picker uses)."""
    if cwd is not None and not _within_root(cwd):
        return []                                     # security: outside allowed root
    db = sqlite3.connect(DB_PATH)
    out = []
    for cmd, c, last_used, rc in db.execute(
            "SELECT cmd_text, cwd, last_used, run_count FROM commands ORDER BY last_used DESC"):
        if cmd.strip().startswith("#"):
            continue
        if not _within_root(c):                        # security boundary
            continue
        if cwd is not None and not _folder_match(c, cwd):
            continue
        safe = _safe_command(cmd)
        if safe is None:
            continue
        out.append({"command": safe, "cwd": c, "run_count": rc, "generated": False})
        if len(out) >= n:
            break
    return out


def reman_failures(cwd: str = None, n: int = 20) -> list:
    """Recent commands that FAILED (real non-zero exit), newest first - the agent's view of what
    recently broke, so it can avoid repeating them or help the user debug. Only commands that
    only ever failed are listed (a command that later succeeded is not a 'failure'). Failed
    commands are retained ~1 day then auto-purged, so this is inherently a recent-failures window.
    'unknown' commands (exit never captured) are NOT failures and are excluded. cwd scopes to that
    exact folder. Real commands only."""
    if cwd is not None and not _within_root(cwd):
        return []                                     # security: outside allowed root
    db = sqlite3.connect(DB_PATH)
    now = time.time()
    out = []
    for cmd, c, last_used, fc, lx in db.execute(
            """SELECT cmd_text, cwd, last_used, fail_count, last_exit FROM commands
               WHERE fail_count > 0 AND success_count = 0 ORDER BY last_used DESC"""):
        if cmd.strip().startswith("#"):
            continue
        if not _within_root(c):                        # security boundary
            continue
        if cwd is not None and not _folder_match(c, cwd):
            continue
        safe = _safe_command(cmd)
        if safe is None:
            continue
        age = "?" if not last_used else ("today" if (now - last_used) < 86400
                                         else f"{int((now - last_used) / 86400)}d ago")
        out.append({"command": safe, "cwd": c, "last_exit": lx, "fail_count": fc,
                    "last_run": age, "generated": False})
        if len(out) >= n:
            break
    return out


def reman_fixes(failed_command: str, cwd: str = None) -> list:
    """For a failed command, return similar commands from the user's successes (did-you-mean).
    Real commands only. cwd scopes to fixes actually run in that exact folder."""
    if cwd is not None and not _within_root(cwd):
        return []                                     # security: outside allowed root
    db = sqlite3.connect(DB_PATH)
    out = []
    for score, typo, sem, cmd in _dym(failed_command, k=20, worked_only=False, here=None):
        folders = [r[0] for r in db.execute("SELECT cwd FROM commands WHERE cmd_text=?", (cmd,))]
        if not any(_within_root(f) for f in folders):              # security boundary
            continue
        if cwd is not None and not any(_folder_match(f, cwd) for f in folders):
            continue
        safe = _safe_command(cmd)
        if safe is None:
            continue
        out.append({"fixed_command": safe, "confidence": round(score, 3),
                    "typo_sim": round(typo, 3), "intent_sim": round(sem, 3), "generated": False})
        if len(out) >= 3:
            break
    return out


def reman_check(command: str, cwd: str = None) -> dict:
    """VET a command before running it. Answers 'has THIS user actually run this, and did it work?'
    from real provenance - run/success/fail counts, last exit, who ran it, which folders - and a
    verdict an agent can act on. This is the thing a raw-history tool cannot do: it grades trust,
    not just 'does this string appear in history'. cwd scopes to that exact folder. If the command
    was never run, returns the nearest commands the user HAS run (so the agent grounds on a real one
    instead of guessing). Verdicts: verified | failed | mixed | ran_unknown | never_run."""
    cmd = (command or "").strip()
    if cwd is not None and not _within_root(cwd):
        return {"command": cmd, "verdict": "denied",
                "advice": "That folder is outside the allowed root for this agent.", "generated": False}
    db = sqlite3.connect(DB_PATH)
    rows = db.execute(
        """SELECT cwd, run_count, success_count, fail_count, last_exit, last_used, last_actor
           FROM commands WHERE cmd_text = ?""", (cmd,)).fetchall()
    rows = [r for r in rows if _within_root(r[0])]          # security boundary
    if cwd is not None:
        rows = [r for r in rows if _folder_match(r[0], cwd)]
    here = " in this folder" if cwd else ""
    if not rows:
        similar = [c["fixed_command"] for c in reman_fixes(cmd, cwd=cwd)]
        if not similar and cwd is not None:                  # nothing similar in-folder -> widen
            similar = [c["fixed_command"] for c in reman_fixes(cmd, cwd=None)]
        return {"command": cmd, "verdict": "never_run", "run_count": 0,
                "advice": f"You have never run this{here}. Prefer a known command below."
                          if similar else f"You have never run this{here}, and nothing similar is known.",
                "similar": similar, "generated": False}
    rc = sum(r[1] or 0 for r in rows); sc = sum(r[2] or 0 for r in rows); fc = sum(r[3] or 0 for r in rows)
    newest = max(rows, key=lambda r: r[5] or 0)
    last_used, last_exit = newest[5], newest[4]
    actors = {(r[6] or "human") for r in rows}
    actor = ("agent" if all(a.startswith("agent") for a in actors)
             else "human" if all(not a.startswith("agent") for a in actors) else "mixed")
    if sc and not fc:
        verdict = "verified"; advice = f"You've run this {rc}x and it succeeded ({sc} ok){here}. Safe to reuse."
    elif fc and not sc:
        verdict = "failed"; advice = f"This only ever FAILED for you ({fc}x, last exit {last_exit}){here}. Fix or avoid."
    elif sc and fc:
        verdict = "mixed"; advice = f"Mixed: {sc} ok / {fc} fail across {rc} runs{here}. Use with caution."
    else:
        verdict = "ran_unknown"; advice = f"You've run this {rc}x{here} but exit codes weren't captured - outcome unknown."
    now = time.time()
    age = "?" if not last_used else ("today" if (now - last_used) < 86400 else f"{int((now - last_used) / 86400)}d ago")
    out = {"command": cmd, "verdict": verdict, "advice": advice, "run_count": rc,
           "success_count": sc, "fail_count": fc, "last_exit": last_exit, "last_run": age,
           "actor": actor, "generated": False}
    if cwd is None:
        out["folders"] = sorted({r[0] for r in rows if r[0]})
    return out


def reman_flows(cwd: str = None, n: int = 15) -> list:
    """Recurring command SEQUENCES the user runs (workflow memory) - chains executed back-to-back
    2+ times, e.g. 'activate -> uvicorn -> cd frontend -> npm run dev'. Lets an agent see HOW the
    user actually operates a project, not just isolated commands, so it can follow their real
    workflow. cwd scopes to that exact folder. Boundary-checked + secrets redacted. Real only."""
    if cwd is not None and not _within_root(cwd):
        return []
    from reman_daemon import client
    # workflows naturally cd across subfolders, so scope by the root SUBTREE (via _cmd_within_root),
    # not one exact folder - build globally, then keep only chains fully inside the allowed root.
    flows = client({"op": "flows", "cwd": None, "k": 120}).get("results", [])
    db = sqlite3.connect(DB_PATH)
    out = []
    for f in flows:
        seq = f.get("sequence", [])
        if not all(_cmd_within_root(db, c) for c in seq):       # every step inside allowed root
            continue
        safe = [_safe_command(c) for c in seq]
        if any(s is None for s in safe):                        # a step still looks secret -> drop flow
            continue
        out.append({"sequence": safe, "count": f["count"], "length": f["length"], "generated": False})
        if len(out) >= n:
            break
    return out


def _build_server():
    from mcp.server.fastmcp import FastMCP
    mcp = FastMCP("reman")
    mcp.tool()(reman_search)
    mcp.tool()(reman_recent)
    mcp.tool()(reman_failures)
    mcp.tool()(reman_fixes)
    mcp.tool()(reman_check)
    mcp.tool()(reman_flows)
    return mcp


if __name__ == "__main__":
    _build_server().run()
