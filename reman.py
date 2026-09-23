#!/usr/bin/env python3
"""
Reman - Phase 0 spike (semantic recall over your own shell history).

Single-file, intentionally minimal. Proves the ONE thing that gates the project:
does semantic search over your real commands beat Ctrl-R?

Deliberately omitted (they belong to later phases, see reman-spec.md):
  variant grouping, hybrid retention, fix-pairs, did-you-mean, MCP, the daemon.
This is a cold CLI, not the daemon - fine for a spike; slow startup doesn't matter
when you're just running the acceptance test by hand.

USAGE
  # 1. backfill from your real Atuin history (VERIFY the format fields first - see R2):
  #    NOTE (Windows/PowerShell): `atuin history list` needs $ATUIN_SESSION set when
  #    invoked outside a hooked shell. Set it first:  $env:ATUIN_SESSION = (atuin uuid)
  atuin history list --format "{time}`t{exit}`t{directory}`t{command}" | python reman.py ingest
  # 2. ask in natural language:
  python reman.py search "that ffmpeg thing that downscaled video"
  #    Seeded-from-PSReadLine data has no exit codes (exit=-1 -> unknown), so the
  #    success-pool filter excludes everything. For the R1 gate on seeded data, use --all:
  python reman.py search --all "deploy the app"

DEPS:  pip install fastembed numpy
  fastembed downloads a small ONNX model on first run (needs network once).
"""
import sqlite3, sys, time, hashlib, argparse, struct, os

DB_PATH = os.environ.get("REMAN_DB", os.path.expanduser("~/.reman/reman.db"))
MODEL   = "BAAI/bge-small-en-v1.5"   # 384-dim, CPU-friendly
DIM     = 384


# ---------- storage ----------
def connect():
    os.makedirs(os.path.dirname(DB_PATH), exist_ok=True)
    db = sqlite3.connect(DB_PATH)
    db.executescript("""
        CREATE TABLE IF NOT EXISTS commands (
            id            INTEGER PRIMARY KEY,
            cmd_text      TEXT,
            cmd_hash      TEXT UNIQUE,
            cwd           TEXT,
            first_seen    INTEGER,
            last_used     INTEGER,
            run_count     INTEGER DEFAULT 1,
            success_count INTEGER DEFAULT 0,
            fail_count    INTEGER DEFAULT 0,
            last_exit     INTEGER
        );
        CREATE TABLE IF NOT EXISTS command_vec (
            command_id INTEGER PRIMARY KEY REFERENCES commands(id),
            vec        BLOB                      -- float32 * DIM, brute-force cosine at read
        );
    """)
    return db


def pack(v):   return struct.pack(f"{DIM}f", *v)
def unpack(b): return struct.unpack(f"{DIM}f", b)


# ---------- embedder (lazy, so tests can inject a stub) ----------
_embedder = None
def embed(text):
    global _embedder
    if _embedder is None:
        from fastembed import TextEmbedding          # lazy import: only when really embedding
        _embedder = TextEmbedding(model_name=MODEL)
    return list(next(_embedder.embed([text])))


def embed_many(texts):
    """Batch embed (much faster than one-at-a-time for backfill)."""
    global _embedder
    if _embedder is None:
        from fastembed import TextEmbedding
        _embedder = TextEmbedding(model_name=MODEL)
    return [list(v) for v in _embedder.embed(texts)]


# ---------- ingestion (T0.1) ----------
def parse_line(line):
    # format: {time}\t{exit}\t{directory}\t{command}  - command LAST so maxsplit keeps tabs in cmd
    parts = line.rstrip("\n").rstrip("\r").split("\t", 3)
    if len(parts) != 4:
        return None
    t, exit_s, cwd, cmd = parts
    if not cmd.strip():
        return None
    try:    exit_code = int(exit_s)
    except ValueError: exit_code = None
    # Atuin uses -1 for "unknown exit" (e.g. PSReadLine-imported rows have no exit code).
    # Treat unknown as None: neither success nor failure, so it never poisons the success pool.
    if exit_code == -1:
        exit_code = None
    if cwd == "unknown":
        cwd = None
    ts = _to_epoch(t)
    return ts, exit_code, cwd, cmd


def _to_epoch(t):
    # Atuin --format {time} is usually a parseable datetime; fall back to "now".
    for fmt in ("%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S"):
        try:    return int(time.mktime(time.strptime(t[:19], fmt)))
        except (ValueError, TypeError): pass
    try:    return int(t)                 # already epoch?
    except (ValueError, TypeError): return int(time.time())


def ingest(stream, embed_fn=None):
    db = connect()
    new_rows = []          # (cid, cmd) pending embedding -> batch at the end
    new, touched = 0, 0
    for line in stream:
        row = parse_line(line)
        if not row:
            continue
        ts, exit_code, cwd, cmd = row
        h = hashlib.sha256(f"{cmd}\x00{cwd}".encode()).hexdigest()
        cur = db.execute("SELECT id, run_count, success_count, fail_count FROM commands WHERE cmd_hash=?", (h,))
        existing = cur.fetchone()
        ok   = 1 if exit_code == 0 else 0
        bad  = 1 if (exit_code is not None and exit_code != 0) else 0
        if existing:                                  # touch-on-use
            cid, rc, sc, fc = existing
            db.execute("""UPDATE commands SET last_used=?, run_count=?, success_count=?,
                          fail_count=?, last_exit=? WHERE id=?""",
                       (ts, rc + 1, sc + ok, fc + bad, exit_code, cid))
            touched += 1
        else:
            cur = db.execute("""INSERT INTO commands
                   (cmd_text, cmd_hash, cwd, first_seen, last_used, run_count,
                    success_count, fail_count, last_exit)
                   VALUES (?,?,?,?,?,1,?,?,?)""",
                   (cmd, h, cwd, ts, ts, ok, bad, exit_code))
            cid = cur.lastrowid
            new_rows.append((cid, cmd))
            new += 1
    # EMBED ON WRITE (once per unique command), batched. Skip if a stub disabled it.
    if new_rows:
        fn = embed_fn
        if fn is None:
            texts = [c for _, c in new_rows]
            vecs = embed_many(texts)
            for (cid, _), v in zip(new_rows, vecs):
                db.execute("INSERT INTO command_vec (command_id, vec) VALUES (?,?)", (cid, pack(v)))
        else:
            for cid, cmd in new_rows:
                db.execute("INSERT INTO command_vec (command_id, vec) VALUES (?,?)", (cid, pack(fn(cmd))))
    db.commit()
    print(f"ingest: {new} new unique commands, {touched} re-runs touched", file=sys.stderr)


# ---------- search (T0.3) ----------
def _cosine(a, b):
    import numpy as np
    a, b = np.asarray(a), np.asarray(b)
    na, nb = np.linalg.norm(a), np.linalg.norm(b)
    return float(a @ b / (na * nb)) if na and nb else 0.0


def search(query, k=10, worked_only=True, embed_fn=embed):
    import numpy as np, math
    db = connect()
    qv = np.asarray(embed_fn(query))
    where = "WHERE c.success_count > 0" if worked_only else ""
    rows = db.execute(f"""
        SELECT c.id, c.cmd_text, c.cwd, c.last_used, c.run_count,
               c.success_count, c.last_exit, v.vec
        FROM commands c JOIN command_vec v ON v.command_id = c.id {where}
    """).fetchall()
    now = time.time()
    scored = []
    for cid, cmd, cwd, last_used, rc, sc, ex, vblob in rows:
        sim = _cosine(qv, unpack(vblob))
        recency = 1.0 / (1.0 + max(0, (now - (last_used or now)) / 86400.0))  # days
        score = sim + 0.05 * math.log(rc + 1) + 0.1 * recency
        scored.append((score, sim, cmd, cwd, last_used, rc, sc))
    scored.sort(reverse=True)
    return scored[:k]


def _fmt_age(ts):
    if not ts: return "?"
    d = (time.time() - ts) / 86400.0
    return f"{d:.0f}d ago" if d >= 1 else "today"


def print_results(results):
    if not results:
        print("(no matches - empty history? run `ingest` first, or try --all)")
        return
    for score, sim, cmd, cwd, last_used, rc, sc in results:
        print(f"\n  {cmd}")
        print(f"    sim={sim:.3f}  runs={rc}  ok={sc}  last={_fmt_age(last_used)}  @ {cwd or '?'}")


# ---------- cli ----------
def main():
    ap = argparse.ArgumentParser(prog="reman", description="Phase 0 spike: semantic command recall")
    sub = ap.add_subparsers(dest="cmd", required=True)
    sub.add_parser("ingest", help="read Atuin history (TSV) from stdin")
    sp = sub.add_parser("search", help="natural-language search of your real commands")
    sp.add_argument("query", nargs="+")
    sp.add_argument("-k", type=int, default=10)
    sp.add_argument("--all", action="store_true", help="include commands that never succeeded (needed for seeded data)")
    args = ap.parse_args()

    if args.cmd == "ingest":
        ingest(sys.stdin)
    elif args.cmd == "search":
        print_results(search(" ".join(args.query), k=args.k, worked_only=not args.all))


if __name__ == "__main__":
    main()
