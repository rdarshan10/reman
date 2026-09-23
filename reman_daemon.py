#!/usr/bin/env python3
"""
Reman T0.5 - daemon + thin client.

WHY: a cold CLI reloads the embedding model every call (~1.3s) AND re-reads + cosines every
vector in a Python loop (~85ms at 1.3k cmds). The daemon pays the model load ONCE and holds an
in-memory, L2-normalised numpy matrix of all command vectors, so each query is: embed the query
(warm) + a single matrix-vector product (microseconds). Terminal speed. The client is a thin
socket call (no model, no torch).

Protocol: one JSON request line in, one JSON response line out, over a localhost TCP socket
(language-agnostic on purpose - the daemon can later be rewritten in Rust without touching any
client). POSIX could use a Unix domain socket; TCP localhost is used for Windows reliability.

  python reman_daemon.py serve                 # load+warm model, build vector index, listen
  python reman_daemon.py search "<intent>"     # thin client query
  python reman_daemon.py didyoumean "<failed>"
  python reman_daemon.py ping
"""
import socket, json, sys, time, threading, re, sqlite3, os
import numpy as np
from reman import DB_PATH, embed, unpack
import reman_enrich as RE

HOST, PORT = "127.0.0.1", 8765
INDEX = None                  # in-memory vector index, built once at serve()
_LOCK = threading.Lock()      # guards INDEX during search reads + incremental ingest updates


def _atuin_db_path():
    """Locate Atuin's local history.db cross-platform (XDG, ~/.local/share, Windows LOCALAPPDATA)."""
    cands = []
    xdg = os.environ.get("XDG_DATA_HOME")
    if xdg:
        cands.append(os.path.join(xdg, "atuin", "history.db"))
    cands.append(os.path.join(os.path.expanduser("~"), ".local", "share", "atuin", "history.db"))
    la = os.environ.get("LOCALAPPDATA")
    if la:
        cands.append(os.path.join(la, "atuin", "history.db"))
    for c in cands:
        if os.path.exists(c):
            return c
    return None


def _meta_get(db, key, default=None):
    db.execute("CREATE TABLE IF NOT EXISTS reman_meta(key TEXT PRIMARY KEY, value TEXT)")
    row = db.execute("SELECT value FROM reman_meta WHERE key=?", (key,)).fetchone()
    return row[0] if row else default


def _meta_set(db, key, value):
    db.execute("CREATE TABLE IF NOT EXISTS reman_meta(key TEXT PRIMARY KEY, value TEXT)")
    db.execute("INSERT OR REPLACE INTO reman_meta(key,value) VALUES(?,?)", (key, str(value)))


_CTRL_RE = re.compile(r"[\x00-\x1f\x7f]")


def _is_junk(cmd):
    """Noise we don't want in recall: blank, control-char-only (the \\x07 beep, \\x15), or 1-char."""
    s = _CTRL_RE.sub("", cmd or "").strip()
    return len(s) < 2


def clean_db():
    """Scrub the corpus: (1) drop junk commands, (2) recompute success/fail/last_exit from the REAL
    per-run exit codes in `executions`, undoing the earlier 'unknown(-1)=fail' pollution. Caller
    should rebuild the index afterwards (rows were deleted). Returns count removed."""
    db = sqlite3.connect(DB_PATH)
    junk = [cid for cid, txt in db.execute("SELECT id, cmd_text FROM commands") if _is_junk(txt)]
    for cid in junk:
        db.execute("DELETE FROM commands WHERE id=?", (cid,))
        db.execute("DELETE FROM command_vec WHERE command_id=?", (cid,))
        db.execute("DELETE FROM command_desc_vec WHERE command_id=?", (cid,))
        db.execute("DELETE FROM executions WHERE command_id=?", (cid,))
    db.execute("""UPDATE commands SET
        success_count = (SELECT COUNT(*) FROM executions e WHERE e.command_id=commands.id AND e.exit=0),
        fail_count    = (SELECT COUNT(*) FROM executions e WHERE e.command_id=commands.id AND e.exit>0),
        last_exit     = (SELECT e.exit FROM executions e WHERE e.command_id=commands.id ORDER BY e.ts DESC LIMIT 1)""")
    db.commit()
    return len(junk)


def purge_failed(days=1):
    """Retention: drop commands that ONLY ever failed (success=0, fail>0 -> status 'fail') and
    haven't been used in `days`. 'unknown' (exit never captured) is kept - only real failures expire.
    Returns count removed. Caller rebuilds the index iff >0."""
    db = sqlite3.connect(DB_PATH)
    cutoff = int(time.time()) - days * 86400
    ids = [r[0] for r in db.execute(
        "SELECT id FROM commands WHERE fail_count > 0 AND success_count = 0 AND last_used < ?", (cutoff,))]
    for cid in ids:
        db.execute("DELETE FROM commands WHERE id=?", (cid,))
        db.execute("DELETE FROM command_vec WHERE command_id=?", (cid,))
        db.execute("DELETE FROM command_desc_vec WHERE command_id=?", (cid,))
        db.execute("DELETE FROM executions WHERE command_id=?", (cid,))
    db.commit()
    return len(ids)


def detect_flows(cwd=None, min_count=2, max_len=4, gap_seconds=300, limit=40):
    """Mine recurring command SEQUENCES from `executions`: runs of commands executed back-to-back
    in the same session (within `gap_seconds`) that recur >= min_count times. This is workflow
    memory - the chains you repeat (backup -> migrate -> up; add -> commit -> push). cwd scopes to
    sequences run in that exact folder. Returns [{sequence, count, length}], longest+most-frequent
    first, with shorter sub-sequences of a returned longer chain dropped."""
    from collections import Counter
    db = sqlite3.connect(DB_PATH)
    # Group by TIME proximity, not session: Atuin's PowerShell hook fragments sessions (≈1 per
    # command), so a run = commands executed back-to-back (gap < gap_seconds), ordered by ts.
    rows = db.execute("""SELECT e.ts, e.cwd, c.cmd_text
                         FROM executions e JOIN commands c ON c.id = e.command_id
                         WHERE c.cmd_text NOT LIKE '#%' AND e.ts IS NOT NULL
                         ORDER BY e.ts""").fetchall()
    grams = Counter()

    def is_periodic(g):                        # A-B-A-B / A-B-C-A-B-C toggles -> not a workflow
        L = len(g)
        for p in range(1, L // 2 + 1):
            if L % p == 0 and all(g[i] == g[i % p] for i in range(L)):
                return True
        return False

    def harvest(buf):
        for n in range(2, max_len + 1):
            for i in range(len(buf) - n + 1):
                g = tuple(buf[i:i + n])
                if len(set(g)) < 2:            # skip pure repeats (ls; ls; ls)
                    continue
                if any(g[j] == g[j + 1] for j in range(len(g) - 1)):   # skip adjacent dup
                    continue
                if len(g) >= 3 and len(set(g)) == 2:   # 2 cmds over 3+ steps = a toggle, not a flow
                    continue
                if is_periodic(g):             # skip longer cycles (A-B-C-A-B-C)
                    continue
                grams[g] += 1

    last_ts, buf = None, []
    for ts, c, cmd in rows:
        if cwd is not None and not _cwd_eq(c, cwd):
            harvest(buf); buf = []; last_ts = ts          # folder scope: break on any outside cmd
            continue
        if last_ts is not None and ts - last_ts > gap_seconds:
            harvest(buf); buf = []                        # time gap -> new run
        buf.append(cmd); last_ts = ts
    harvest(buf)

    flows = [{"sequence": list(g), "count": n, "length": len(g)}
             for g, n in grams.items() if n >= min_count]
    flows.sort(key=lambda f: (f["length"], f["count"]), reverse=True)
    kept = []
    for f in flows:                            # drop a shorter chain fully contained in a kept one
        s = f["sequence"]
        if any(_is_subseq(s, k["sequence"]) for k in kept):
            continue
        kept.append(f)
    kept.sort(key=lambda f: (f["count"], f["length"]), reverse=True)
    return kept[:limit]


def _cwd_eq(a, b):
    if not a or not b:
        return False
    return os.path.normcase(os.path.normpath(a)) == os.path.normcase(os.path.normpath(b))


def _is_subseq(short, long):
    """True if `short` is a contiguous slice of `long`."""
    if len(short) >= len(long):
        return False
    return any(long[i:i + len(short)] == short for i in range(len(long) - len(short) + 1))


def _status_of(sc, fc):
    """pass/fail label from real exit counts. unknown = no exit code was ever captured."""
    sc = sc or 0; fc = fc or 0
    if sc and not fc:
        return "ok"
    if fc and not sc:
        return "fail"
    if sc and fc:
        return "mixed"
    return "unknown"


def _attach_status(db, results):
    """Annotate search results with pass/fail status from real exit counts (one batched lookup)."""
    cmds = [r["command"] for r in results]
    if not cmds:
        return
    qm = ",".join("?" * len(cmds))
    st = {}
    for c, sc, fc in db.execute(
            f"SELECT cmd_text, SUM(success_count), SUM(fail_count) FROM commands "
            f"WHERE cmd_text IN ({qm}) GROUP BY cmd_text", cmds):
        st[c] = _status_of(sc, fc)
    for r in results:
        r["status"] = st.get(r["command"], "unknown")


def sync_atuin(limit=500):
    """Pull NEW live commands from Atuin's history.db into reman.db as human runs. Incremental:
    a cursor on Atuin's nanosecond timestamp means we only ever read rows we haven't seen. Atuin
    already captures cwd + exit for every command, so reman gets folder + success for free.
    Returns (ingested, remaining, error)."""
    apath = _atuin_db_path()
    if not apath:
        return (0, 0, "atuin db not found")
    from reman_actor import migrate_actor, record_run
    from reman_enrich import migrate as enrich_migrate
    db = sqlite3.connect(DB_PATH)
    enrich_migrate(db); migrate_actor(db)
    cursor = int(_meta_get(db, "atuin_cursor", "0"))
    adb = sqlite3.connect(apath)
    rows = adb.execute(
        """SELECT timestamp, exit, cwd, command, session FROM history
           WHERE timestamp > ? AND deleted_at IS NULL AND command NOT LIKE '#%'
           ORDER BY timestamp ASC LIMIT ?""", (cursor, int(limit))).fetchall()
    n = 0; maxts = cursor
    for ts, exitc, cwd, cmd, sess in rows:
        cmd = (cmd or "").strip()
        if cmd and not _is_junk(cmd):
            cid = record_run(db, cmd, exitc if exitc is not None else 0, cwd, sess or "",
                             "human", ts=int(ts // 1_000_000_000), embed_fn=embed)
            with _LOCK:
                index_upsert(cid)
            n += 1
        if ts > maxts:
            maxts = ts
    if rows:
        _meta_set(db, "atuin_cursor", maxts)
        db.commit()
    remaining = adb.execute(
        """SELECT COUNT(*) FROM history WHERE timestamp > ? AND deleted_at IS NULL
           AND command NOT LIKE '#%'""", (maxts,)).fetchone()[0]
    return (n, remaining, None)


def _ensure_indexes(db):
    """Indexes for the hot query paths (GROUP BY cmd_text, ORDER BY last_used, cwd/actor filters,
    execution joins). Cheap now (~ms), keeps browse/scope fast as the history grows large."""
    for stmt in (
        "CREATE INDEX IF NOT EXISTS idx_cmd_text ON commands(cmd_text)",
        "CREATE INDEX IF NOT EXISTS idx_last_used ON commands(last_used)",
        "CREATE INDEX IF NOT EXISTS idx_cwd ON commands(cwd)",
        "CREATE INDEX IF NOT EXISTS idx_exec_cmd ON executions(command_id)",
    ):
        db.execute(stmt)
    db.commit()


def _rebuild_index():
    """Rebuild the in-memory index from the DB (used after clean deletes rows). Holds _LOCK."""
    global INDEX
    with _LOCK:
        INDEX = build_index()


def build_index():
    """Load all command vectors into normalised numpy matrices once (raw + description).
    Precompute group_key, run_count, last_used, freq-prior so per-query work is just a matmul."""
    db = sqlite3.connect(DB_PATH)
    rows = db.execute("""SELECT c.id, c.cmd_text, c.description, c.run_count, c.last_used, cv.vec
                         FROM commands c JOIN command_vec cv ON cv.command_id = c.id""").fetchall()
    n = len(rows)
    texts, descs, gkeys = [], [], []
    raw = np.empty((n, 384), dtype=np.float32)
    rc = np.empty(n, dtype=np.float32)
    lu = np.empty(n, dtype=np.float64)
    idpos = {}
    for i, (cid, cmd, desc, run_count, last_used, vec) in enumerate(rows):
        idpos[cid] = i
        texts.append(cmd); descs.append(desc); gkeys.append(RE.group_key(cmd))
        raw[i] = unpack(vec)
        rc[i] = run_count or 1
        lu[i] = last_used or time.time()
    raw /= (np.linalg.norm(raw, axis=1, keepdims=True) + 1e-9)
    # description vectors (gen+spec); keep owner index for per-command max-blend
    dvecs, downer = [], []
    for cid, vec in db.execute("SELECT command_id, vec FROM command_desc_vec"):
        if cid in idpos:
            dvecs.append(unpack(vec)); downer.append(idpos[cid])
    if dvecs:
        desc = np.array(dvecs, dtype=np.float32)
        desc /= (np.linalg.norm(desc, axis=1, keepdims=True) + 1e-9)
        downer = np.array(downer, dtype=np.int64)
    else:
        desc = np.zeros((0, 384), dtype=np.float32); downer = np.zeros(0, dtype=np.int64)
    return {"n": n, "texts": texts, "descs": descs, "gkeys": gkeys, "raw": raw,
            "desc": desc, "downer": downer, "rc": rc, "lu": lu, "idpos": idpos,
            "freq": 0.03 * (rc / (rc + 20.0)),
            "is_comment": np.array([t.strip().startswith("#") for t in texts])}


def index_upsert(cid):
    """Incrementally fold ONE command into the in-memory index after an ingest - no full rebuild.
    Existing command -> bump run_count/recency/freq in place. New command -> append its vector +
    metadata. Caller holds _LOCK."""
    idx = INDEX
    if idx is None:
        return
    db = sqlite3.connect(DB_PATH)
    row = db.execute("SELECT cmd_text, description, run_count, last_used FROM commands WHERE id=?", (cid,)).fetchone()
    if not row:
        return
    cmd, desc, rc, lu = row
    rc = rc or 1
    lu = lu or time.time()
    if cid in idx["idpos"]:                              # existing -> update counts only (text unchanged)
        p = idx["idpos"][cid]
        idx["rc"][p] = rc
        idx["lu"][p] = lu
        idx["freq"][p] = 0.03 * (rc / (rc + 20.0))
        return
    vrow = db.execute("SELECT vec FROM command_vec WHERE command_id=?", (cid,)).fetchone()
    if not vrow:
        return
    v = np.asarray(unpack(vrow[0]), dtype=np.float32)
    v /= (np.linalg.norm(v) + 1e-9)
    p = idx["n"]
    idx["idpos"][cid] = p
    idx["texts"].append(cmd); idx["descs"].append(desc); idx["gkeys"].append(RE.group_key(cmd))
    idx["raw"] = np.vstack([idx["raw"], v])
    idx["rc"] = np.append(idx["rc"], rc)
    idx["lu"] = np.append(idx["lu"], lu)
    idx["freq"] = np.append(idx["freq"], 0.03 * (rc / (rc + 20.0)))
    idx["is_comment"] = np.append(idx["is_comment"], bool(cmd.strip().startswith("#")))
    dvs = db.execute("SELECT vec FROM command_desc_vec WHERE command_id=?", (cid,)).fetchall()
    if dvs:                                              # usually none for a fresh agent capture (no desc yet)
        nd = np.array([unpack(x[0]) for x in dvs], dtype=np.float32)
        nd /= (np.linalg.norm(nd, axis=1, keepdims=True) + 1e-9)
        idx["desc"] = np.vstack([idx["desc"], nd]) if idx["desc"].shape[0] else nd
        idx["downer"] = np.append(idx["downer"], np.full(len(dvs), p, dtype=np.int64))
    idx["n"] += 1


def fast_search(query, k=5):
    idx = INDEX
    q = np.asarray(embed(query), dtype=np.float32)
    q /= (np.linalg.norm(q) + 1e-9)
    raw_sim = idx["raw"] @ q                              # (n,) one matmul
    dsim = np.full(idx["n"], -1.0, dtype=np.float32)
    if idx["desc"].shape[0]:
        np.maximum.at(dsim, idx["downer"], idx["desc"] @ q)
    sim = np.maximum(raw_sim, dsim)
    now = time.time()
    recency = 0.02 / (1.0 + np.maximum(0.0, (now - idx["lu"]) / 86400.0))
    score = sim + idx["freq"] + recency
    score[idx["is_comment"]] = -1e9                       # comments are not runnable commands
    order = np.argsort(-score)

    texts, descs, gkeys, rc = idx["texts"], idx["descs"], idx["gkeys"], idx["rc"]
    qtokens = [w for w in re.findall(r"[a-z0-9]+", query.lower()) if len(w) > 2 and w not in RE.STOP]
    out, seen, best_sim, lit_hit, looked = [], set(), None, False, 0
    for i in order:
        if idx["is_comment"][i]:
            continue
        if best_sim is None:
            best_sim = float(sim[i])
        if looked < 8:                                    # literal-overlap check over the top hits
            blob = (texts[i] + " " + (descs[i] or "")).lower()
            if any(w in blob for w in qtokens):
                lit_hit = True
            looked += 1
        gk = gkeys[i]
        if gk in seen:
            continue
        seen.add(gk)
        out.append({"command": texts[i], "similarity": round(float(sim[i]), 3),
                    "runs": int(rc[i]), "intent": gk, "description": descs[i]})
        if len(out) >= k:
            break

    confident = (best_sim is not None) and best_sim >= RE.WEAK_SIM and (not qtokens or lit_hit)
    if not confident:                                     # manual literal fallback (user rule)
        words = [w.lower() for w in query.split() if len(w) > 1]
        lit = []
        for i in range(idx["n"]):
            if idx["is_comment"][i]:
                continue
            low = texts[i].lower()
            m = sum(1 for w in words if w in low)
            if m:
                lit.append((m, int(rc[i]), texts[i]))
        lit.sort(reverse=True)
        return {"mode": "manual", "results": [{"command": c, "matched_terms": m, "runs": r}
                                              for m, r, c in lit[:k]]}
    from reman_actor import provenance                      # Phase 2 inline provenance
    pr = provenance(sqlite3.connect(DB_PATH), [r["command"] for r in out])
    for r in out:
        r.update(pr.get(r["command"], {}))
    return {"mode": "semantic", "results": out}


def handle(req):
    op = req.get("op")
    if op == "ping":
        return {"ok": True, "indexed": INDEX["n"] if INDEX else 0}
    if op == "ingest":
        # actor-tagged push ingest using the WARM model (the hook never loads it itself).
        cmd = (req.get("command") or "").strip()
        if not cmd:
            return {"ok": False, "skipped": "empty"}
        from reman_actor import migrate_actor, record_run
        from reman_enrich import migrate as enrich_migrate
        db = sqlite3.connect(DB_PATH)
        enrich_migrate(db); migrate_actor(db)
        cid = record_run(db, cmd, req.get("exit", 0), req.get("cwd"), req.get("session", ""),
                         req.get("actor", "agent:claude-code"), embed_fn=embed)
        with _LOCK:
            index_upsert(cid)                          # incremental: fold in just this command
        return {"ok": True}
    if op == "sync":
        n, remaining, err = sync_atuin(limit=req.get("limit", 500))
        if err:
            return {"ok": False, "error": err}
        purged = purge_failed(req.get("fail_ttl_days", 1))   # expire commands that only ever failed
        if purged:
            _rebuild_index()
        return {"ok": True, "ingested": n, "remaining": remaining, "purged": purged}
    if op == "clean":
        removed = clean_db()
        _rebuild_index()
        return {"ok": True, "removed": removed}
    if op == "flows":
        return {"results": detect_flows(cwd=req.get("cwd"), min_count=req.get("min_count", 2),
                                        limit=req.get("k", 40))}
    if op == "describe":
        # one selected command's plain-English description (tldr) for the picker footer - lazy,
        # so the browse list stays a lean command/actor/status payload.
        db = sqlite3.connect(DB_PATH)
        row = db.execute("SELECT description FROM commands WHERE cmd_text=? AND description IS NOT NULL "
                         "AND description != '' LIMIT 1", (req.get("command", ""),)).fetchone()
        return {"description": (row[0] if row else "")}
    if op == "search":
        if INDEX is None:
            return {"error": "index not built (run via serve)"}
        scope = req.get("cwd"); actor_f = req.get("actor"); status_f = req.get("status"); want = req.get("k", 5)
        # rank over a wide candidate pool when any filter is active, so sparse subsets still surface.
        with _LOCK:
            res = fast_search(req.get("query", ""), k=(300 if (scope or actor_f or status_f) else want))
        results = res.get("results", [])
        seen = set(); deduped = []                       # de-dupe identical commands (across folders)
        for r in results:
            if r["command"] in seen:
                continue
            seen.add(r["command"]); deduped.append(r)
        results = deduped
        db = sqlite3.connect(DB_PATH)
        if scope:
            allowed = set(r[0] for r in db.execute("SELECT cmd_text FROM commands WHERE cwd = ?", (scope,)))
            results = [r for r in results if r["command"] in allowed]
        if actor_f == "agent":
            results = [r for r in results if str(r.get("actor") or "").startswith("agent")]
        elif actor_f == "human":
            results = [r for r in results if not str(r.get("actor") or "").startswith("agent")]
        _attach_status(db, results)                      # add pass/fail status, then filter
        if status_f == "ok":
            results = [r for r in results if r.get("status") in ("ok", "mixed")]
        elif status_f == "fail":
            results = [r for r in results if r.get("status") in ("fail", "mixed")]
        res["results"] = results[:want]
        return res
    if op == "recent":
        # browse view: real commands newest-first, DE-DUPED across folders (one row per command),
        # with actor + pass/fail status aggregated in the SAME query (scales to the whole history).
        # scope + actor + status are clean filters. k<=0 => all.
        db = sqlite3.connect(DB_PATH)
        scope = req.get("cwd"); actor_f = req.get("actor"); status_f = req.get("status"); k = req.get("k", 40)
        where = ["cmd_text NOT LIKE '#%'"]; params = []
        if scope:
            where.append("cwd = ?"); params.append(scope)
        if actor_f == "agent":
            where.append("last_actor LIKE 'agent:%'")
        elif actor_f == "human":
            where.append("(last_actor IS NULL OR last_actor NOT LIKE 'agent:%')")
        having = ""
        if status_f == "ok":
            having = "HAVING SUM(success_count) > 0"
        elif status_f == "fail":
            having = "HAVING SUM(fail_count) > 0 AND SUM(success_count) = 0"
        limit_sql = "" if (not k or k <= 0) else f" LIMIT {int(k)}"
        # lean payload: the picker only needs command/actor/status. Dropping description (long) +
        # success_rate here roughly halves the JSON for a full-history browse (1000s of rows).
        rows = db.execute(f"""
            SELECT cmd_text,
                   MAX(CASE WHEN last_actor LIKE 'agent:%' THEN 1 ELSE 0 END) AS has_agent,
                   SUM(success_count) AS sc, SUM(fail_count) AS fc,
                   MAX(last_used) AS lu, MAX(run_count) AS rcm
            FROM commands WHERE {" AND ".join(where)}
            GROUP BY cmd_text {having}
            ORDER BY lu DESC, rcm DESC{limit_sql}""", params).fetchall()
        out = [{"command": cmd, "actor": "agent" if ha else "human", "status": _status_of(sc or 0, fc or 0)}
               for cmd, ha, sc, fc, lu, rcm in rows]
        return {"results": out}
    if op == "didyoumean":
        rows = RE.did_you_mean(req.get("query", ""), k=req.get("k", 8),
                               worked_only=req.get("worked_only", False), here=req.get("here"))
        out = [{"command": c, "similarity": round(s, 3), "typo": round(t, 3), "intent_sim": round(se, 3)}
               for s, t, se, c in rows]
        from reman_actor import provenance
        pr = provenance(sqlite3.connect(DB_PATH), [r["command"] for r in out])
        for r in out:
            r.update(pr.get(r["command"], {}))
        return {"results": out}
    return {"error": f"unknown op {op!r}"}


def _serve_conn(conn):
    try:
        data = b""
        while not data.endswith(b"\n"):
            chunk = conn.recv(65536)
            if not chunk:
                break
            data += chunk
        if data.strip():
            conn.sendall((json.dumps(handle(json.loads(data.decode())), default=str) + "\n").encode())
    except Exception as e:
        try:
            conn.sendall((json.dumps({"error": str(e)}) + "\n").encode())
        except Exception:
            pass
    finally:
        conn.close()


def serve():
    global INDEX
    t0 = time.time()
    embed("warmup query to load the model once")
    _ensure_indexes(sqlite3.connect(DB_PATH))   # hot-path indexes (idempotent)
    purge_failed(1)                      # drop stale failed commands before building the index
    INDEX = build_index()
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind((HOST, PORT)); s.listen(16)
    print(f"reman daemon: warm + {INDEX['n']} vectors indexed in {time.time()-t0:.1f}s, "
          f"listening on {HOST}:{PORT}", flush=True)
    while True:
        conn, _ = s.accept()
        threading.Thread(target=_serve_conn, args=(conn,), daemon=True).start()


def _client_once(req, timeout):
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.settimeout(timeout); s.connect((HOST, PORT))
    s.sendall((json.dumps(req) + "\n").encode())
    data = b""
    while not data.endswith(b"\n"):
        chunk = s.recv(65536)
        if not chunk:
            break
        data += chunk
    s.close()
    return json.loads(data.decode())


def _spawn_daemon():
    """Start the daemon detached + WINDOWLESS, cross-platform (Windows + macOS + Linux)."""
    import subprocess
    exe = sys.executable
    kw = dict(stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    if os.name == "nt":
        # DETACHED_PROCESS and CREATE_NO_WINDOW are mutually exclusive -> combining them makes
        # Windows pop a visible console. Use the windowless interpreter (pythonw.exe) so no
        # console is ever created, and DETACHED_PROCESS alone so the daemon outlives the shell.
        pyw = os.path.join(os.path.dirname(exe), "pythonw.exe")
        if os.path.exists(pyw):
            exe = pyw
        kw["creationflags"] = 0x00000008                 # DETACHED_PROCESS
    else:
        kw["start_new_session"] = True                   # POSIX: detach from the shell
    subprocess.Popen([exe, os.path.abspath(__file__), "serve"], **kw)


def _to_clipboard(text):
    """Best-effort cross-platform clipboard copy (Windows clip / macOS pbcopy / Linux xclip)."""
    import subprocess
    if os.name == "nt":
        cmd = ["clip"]
    elif sys.platform == "darwin":
        cmd = ["pbcopy"]
    else:
        cmd = ["xclip", "-selection", "clipboard"]
    p = subprocess.Popen(cmd, stdin=subprocess.PIPE)
    p.communicate(text.encode("utf-8"))


def client(req, timeout=10, autostart=True):
    try:
        return _client_once(req, timeout)
    except (ConnectionRefusedError, OSError):
        if not autostart:
            raise
        _spawn_daemon()                                  # daemon not up -> start it, then wait
        for _ in range(48):
            time.sleep(0.25)
            try:
                return _client_once(req, timeout)
            except (ConnectionRefusedError, OSError):
                continue
        raise


def setup():
    """One-command install: check Atuin, wire the shell integration (idempotent), run an initial
    sync, and print the MCP registration + security note. Safe to re-run."""
    py = sys.executable
    here = os.path.dirname(os.path.abspath(__file__))
    init = os.path.join(here, "reman_init.py")
    print("reman setup\n" + "=" * 46)
    apath = _atuin_db_path()
    print(f"  atuin history.db : {('found  ' + apath) if apath else 'NOT FOUND - install atuin then `atuin import <shell>`'}")

    if os.name == "nt":
        import subprocess
        try:
            prof = subprocess.run(["powershell", "-NoProfile", "-Command", "$PROFILE"],
                                  capture_output=True, text=True, timeout=20).stdout.strip()
        except Exception:
            prof = ""
        raw = open(prof, "rb").read() if (prof and os.path.exists(prof)) else b""
        if prof and b"\x00" in raw[:400]:
            print(f"  powershell profile: {prof} looks UTF-16 - add the reman line manually (avoid corruption)")
        elif prof:
            existing = raw.decode("utf-8-sig", "ignore")
            if "reman_init" in existing:
                print(f"  powershell profile: already wired  ({prof})")
            else:
                block = ("\n# Reman shell integration (added by `reman setup`)\n"
                         f'$_remanPy = "{py}"; $_remanInit = "{init}"\n'
                         "if (Get-Command atuin -ErrorAction SilentlyContinue) { atuin init powershell | Out-String | Invoke-Expression }\n"
                         "if ((Test-Path $_remanPy) -and (Test-Path $_remanInit)) { & $_remanPy $_remanInit powershell | Out-String | Invoke-Expression }\n")
                os.makedirs(os.path.dirname(prof), exist_ok=True)
                with open(prof, "a", encoding="utf-8") as f:
                    f.write(block)
                print(f"  powershell profile: WIRED  ({prof})  -> reload with: . $PROFILE")
        else:
            print("  powershell profile: could not resolve $PROFILE; wire it manually")
    else:
        shell = os.environ.get("SHELL", "")
        kind = "zsh" if "zsh" in shell else "bash"
        rc = "~/.zshrc" if kind == "zsh" else "~/.bashrc"
        print(f"  shell ({kind}): add this line to {rc} (needs fzf on PATH):")
        print(f'      eval "$({py} {init} {kind})"')

    try:
        total = 0
        while True:
            r = client({"op": "sync", "limit": 500})
            if not r.get("ok"):
                break
            got = r.get("ingested", 0); total += got
            if r.get("remaining", 0) <= 0 or got == 0:
                break
        print(f"  initial sync     : ingested {total} commands from Atuin (daemon warm)")
    except Exception as e:
        print(f"  initial sync     : skipped ({e})")

    print(f"  MCP (for agents) : register, then set the security boundary:")
    print(f'      claude mcp add reman -- "{py}" "{os.path.join(here, "reman_mcp.py")}"')
    print("      set env REMAN_MCP_ROOT=<project dir>   (agents only see commands in there)")
    print("      optional: REMAN_MCP_STRICT_SECRETS=1   (drop still-secret-looking commands)")
    print("\n  done - reload your shell to finish.")


def main():
    if len(sys.argv) < 2:
        print("usage: reman_daemon.py [serve | setup | search <q> | flows | stats | "
              "sync | clean | export | ping]"); return
    op = sys.argv[1]
    if op == "setup":
        setup(); return
    if op == "serve":
        serve(); return
    if op == "sync":
        # backfill / catch up: pull all new Atuin history into reman.db (runs in the warm daemon)
        total = 0
        while True:
            resp = client({"op": "sync", "limit": 500})
            if not resp.get("ok"):
                print("sync failed:", resp.get("error")); break
            got = resp.get("ingested", 0); total += got
            print(f"  +{got} (remaining {resp.get('remaining', 0)})")
            if resp.get("remaining", 0) <= 0 or got == 0:
                break
        print(f"reman sync: ingested {total} commands from Atuin")
        return
    if op == "clean":
        resp = client({"op": "clean"})
        print(f"reman clean: removed {resp.get('removed', 0)} junk commands; "
              f"recomputed pass/fail from real exit codes")
        return
    if op == "flows":
        # recurring command sequences (workflow memory). --here scopes to the current folder.
        cwd = os.getcwd() if "--here" in sys.argv[2:] else None
        flows = client({"op": "flows", "cwd": cwd, "k": 25}).get("results", [])
        if not flows:
            print("reman flows: no recurring sequences yet (need commands run back-to-back, 2+ times)")
            return
        for f in flows:
            print(f"  [{f['count']}x] " + "  ->  ".join(f["sequence"]))
        return
    if op == "export":
        # dump commands (one per line) for bulk extraction. flags: --here --actor --status --query --clip
        import argparse
        ap = argparse.ArgumentParser(prog="reman export")
        ap.add_argument("--here", action="store_true", help="only commands run in the current folder")
        ap.add_argument("--actor", choices=["human", "agent"])
        ap.add_argument("--status", choices=["ok", "fail"])
        ap.add_argument("--query", default="", help="semantic filter instead of full history")
        ap.add_argument("--clip", action="store_true", help="copy to clipboard instead of stdout")
        a = ap.parse_args(sys.argv[2:])
        req = {"op": "search", "query": a.query, "k": 200} if a.query.strip() else {"op": "recent", "k": 0}
        if a.here:
            req["cwd"] = os.getcwd()
        if a.actor:
            req["actor"] = a.actor
        if a.status:
            req["status"] = a.status
        cmds = [r["command"] for r in client(req).get("results", [])]
        text = "\n".join(cmds)
        if a.clip:
            _to_clipboard(text)
            print(f"reman export: copied {len(cmds)} commands to clipboard", file=sys.stderr)
        else:
            print(text)
        return
    if op == "complete":
        # plain output for the inline shell picker: top-k real commands, one per line, no decoration
        resp = client({"op": "search", "query": " ".join(sys.argv[2:]), "k": 8})
        for r in resp.get("results", []):
            print(r["command"])
        return
    if op == "recentfull":
        resp = client({"op": "recent", "k": 40})
        for r in resp.get("results", []):
            desc = (r.get("description") or "").replace("\t", " ").replace("\n", " ")[:72]
            sr = r.get("success_rate")
            print("\t".join([r["command"].replace("\t", " "), desc,
                             str(r.get("actor") or "human"), "" if sr is None else str(sr)]))
        return
    if op in ("completefull", "fixesfull"):
        # tab-separated rows for the inline fzf picker: command \t description \t actor \t success
        inner = "search" if op == "completefull" else "didyoumean"
        resp = client({"op": inner, "query": " ".join(sys.argv[2:]), "k": 25})
        for r in resp.get("results", []):
            desc = (r.get("description") or "").replace("\t", " ").replace("\n", " ")[:72]
            sr = r.get("success_rate")
            print("\t".join([r["command"].replace("\t", " "), desc,
                             str(r.get("actor") or "human"), "" if sr is None else str(sr)]))
        return
    t = time.time()
    resp = client({"op": op, "query": " ".join(sys.argv[2:]), "k": 5})
    print(f"[warm round-trip: {(time.time()-t)*1000:.1f} ms]")
    print(json.dumps(resp, indent=2, default=str)[:1500])


if __name__ == "__main__":
    main()
