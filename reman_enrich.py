#!/usr/bin/env python3
"""
Reman Phase 0.5 (T0.5a) - NO-LLM, NO-EXECUTION description enrichment + blended search.

Descriptions come from a BUNDLED OFFLINE dataset (tldr-pages), parsed from markdown.
Nothing is ever executed: we do not run --help, we do not run any binary. Reman stays
a passive retrieval layer (read history, suggest real commands) - it never executes.

Source per command (most specific first), all pure data lookup:
  1. tldr example gloss for (program, subcommand)  e.g. ("docker-compose","down")
  2. tldr subcommand page  e.g. git-commit.md
  3. tldr command page gloss  e.g. alembic.md -> "Database migration tool for SQLAlchemy"
  else -> desc_source='none', raw-command vector only (today's behaviour).

USAGE
  python reman_enrich.py enrich          # build tldr map (once), describe + embed
  python reman_enrich.py search "<q>"    # blended raw+desc search
"""
import sqlite3, os, sys, time, re, json, glob
import numpy as np
from collections import defaultdict, Counter
from reman import DB_PATH, DIM, embed, embed_many, pack, unpack
from reman_fixpairs import norm_edit

WEAK_SIM = 0.66   # bge sims run HOT (gibberish ~0.70), so absolute sim alone can't gate.
                  # Confidence = decent sim AND literal token-overlap (semantics can't separate
                  # keyboard-mash from a real match, but shared content words can). See margin_diag.
STOP = {"the", "that", "this", "thing", "with", "from", "into", "your", "you", "and", "for",
        "over", "via", "a", "an", "of", "to", "in", "on", "my", "it", "is", "all", "some"}

HERE       = os.path.dirname(os.path.abspath(__file__))
TLDR_DIR   = os.path.join(HERE, "tldr_pages")
MAP_CACHE  = os.path.join(HERE, "tldr_map.json")

ALIASES = {"pip3": "pip", "python3": "python", "python3.8": "python", "py": "python",
           "docker-compose": "docker-compose"}

NON_TOOLS = {"#", "&", "cd", "if", "for", "while", "function", "async", "from", "import",
             "echo", "cls", "clear", "$", "{", "}", "(", ")", "set", "return"}


# ---------- schema ----------
def migrate(db):
    cols = [r[1] for r in db.execute("PRAGMA table_info(commands)")]
    if "description" not in cols:
        db.execute("ALTER TABLE commands ADD COLUMN description TEXT")
    if "desc_source" not in cols:
        db.execute("ALTER TABLE commands ADD COLUMN desc_source TEXT DEFAULT 'none'")
    # Fix A: multiple description vectors per command (kind='gen'|'spec'), max-blended at query.
    dvcols = [r[1] for r in db.execute("PRAGMA table_info(command_desc_vec)")]
    if dvcols and "kind" not in dvcols:
        db.execute("DROP TABLE command_desc_vec")
        dvcols = []
    db.execute("""CREATE TABLE IF NOT EXISTS command_desc_vec
                  (command_id INTEGER, kind TEXT, vec BLOB, PRIMARY KEY(command_id, kind))""")
    db.commit()


# ---------- tldr parsing (pure data, no execution) ----------
def _clean_example(cmd):
    cmd = re.sub(r'\{\{.*?\}\}', '', cmd)      # drop {{placeholders}}
    cmd = re.sub(r'\[\[.*?\]\]', '', cmd)
    return cmd.strip().strip('`').strip()


def build_tldr_map():
    """Returns dict: {'cmd': {stem: gloss}, 'ex': {'prog\x00sub': desc}}."""
    cmd_gloss, ex_gloss = {}, {}
    for path in glob.glob(os.path.join(TLDR_DIR, "**", "*.md"), recursive=True):
        stem = os.path.basename(path)[:-3].lower()
        try:
            lines = open(path, encoding="utf-8").read().splitlines()
        except Exception:
            continue
        header_words, gloss_parts, pend = [], [], None
        for ln in lines:
            s = ln.strip()
            if s.startswith("# "):
                header_words = s[2:].strip().lower().split()
            elif s.startswith("> "):
                g = s[2:].strip()
                if not g.lower().startswith("more information"):
                    gloss_parts.append(g.rstrip("."))
            elif s.startswith("- ") and s.endswith(":"):
                pend = s[2:-1].strip()
            elif s.startswith("`") and pend:
                ex = _clean_example(s)
                toks = ex.split()
                # strip the page's command prefix to find the subcommand
                hw = header_words
                if hw and [t.lower() for t in toks[:len(hw)]] == hw:
                    rest = toks[len(hw):]
                else:
                    rest = toks[1:]
                sub = next((t.lower() for t in rest if not t.startswith("-")), None)
                if sub:
                    ex_gloss.setdefault(f"{stem}\x00{sub}", pend)
                pend = None
        if gloss_parts:
            cmd_gloss[stem] = "; ".join(gloss_parts)
    return {"cmd": cmd_gloss, "ex": ex_gloss}


def load_map():
    if os.path.exists(MAP_CACHE):
        return json.load(open(MAP_CACHE, encoding="utf-8"))
    m = build_tldr_map()
    json.dump(m, open(MAP_CACHE, "w", encoding="utf-8"))
    return m


# ---------- command parsing ----------
def parse_cmd(cmd):
    toks = cmd.strip().split()
    i = 0
    while i < len(toks):
        t, tl = toks[i], toks[i].lower()
        if t in ("&", "&&", "|", ";") or re.match(r'^[a-z]:;?$', tl) or tl in ("cd", "set") or t.endswith(";"):
            i += 1
            continue
        break
    if i >= len(toks):
        return None, None
    prog = os.path.basename(toks[i].strip('"\'').replace("\\", "/")).lower()
    if prog.endswith(".exe"):
        prog = prog[:-4]
    prog = ALIASES.get(prog, prog)
    sub = None
    for t in toks[i + 1:]:
        if t.startswith("-"):
            break
        if "/" not in t and "\\" not in t and "=" not in t and not t.startswith('"'):
            sub = t.lower()
            break
    return prog, sub


# ---------- describer (pure lookup) ----------
def describe(cmd, tmap):
    """Returns {'gen': <program gloss>, 'spec': <subcommand gloss>} (either may be absent).
    Fix A: gen and spec are kept SEPARATE so they can be embedded independently and
    max-blended, instead of concatenated (which averages/dilutes the specific signal)."""
    prog, sub = parse_cmd(cmd)
    if not prog or prog in NON_TOOLS:
        return {}
    cmd_gloss, ex_gloss = tmap["cmd"], tmap["ex"]
    out = {}
    if prog in cmd_gloss:
        out["gen"] = f"{prog}: {cmd_gloss[prog]}"
    spec = []
    if sub:
        if f"{prog}-{sub}" in cmd_gloss:
            spec.append(cmd_gloss[f"{prog}-{sub}"])
        ex = ex_gloss.get(f"{prog}\x00{sub}")
        if ex:
            spec.append(ex)
    seen, uniq = set(), []
    for p in spec:
        if p.lower() not in seen:
            seen.add(p.lower()); uniq.append(p)
    if uniq:
        out["spec"] = f"{prog}: " + "; ".join(uniq)
    return out


# ---------- enrich ----------
def enrich():
    db = sqlite3.connect(DB_PATH)
    migrate(db)
    tmap = load_map()
    rows = db.execute("""SELECT id, cmd_text FROM commands
                         WHERE description IS NULL OR desc_source='none' OR desc_source IS NULL""").fetchall()
    todo, n_desc = [], 0           # todo: (command_id, kind, text)
    for cid, cmd in rows:
        d = describe(cmd, tmap)
        disp = d.get("spec") or d.get("gen")     # show the most specific gloss
        db.execute("UPDATE commands SET description=?, desc_source=? WHERE id=?",
                   (disp, "tldr" if disp else "none", cid))
        if disp:
            n_desc += 1
        for kind, text in d.items():
            todo.append((cid, kind, text))
    db.commit()
    if todo:
        vecs = embed_many([t for _, _, t in todo])
        for (cid, kind, _), v in zip(todo, vecs):
            db.execute("INSERT OR REPLACE INTO command_desc_vec (command_id, kind, vec) VALUES (?,?,?)",
                       (cid, kind, pack(v)))
    db.commit()
    print(f"enrich: described {n_desc} commands from tldr ({len(todo)} desc vectors: gen+spec), "
          f"{len(rows)-n_desc} left raw-only; no execution; "
          f"map has {len(tmap['cmd'])} pages / {len(tmap['ex'])} subcommand glosses", file=sys.stderr)


# ---------- blended search ----------
def _cos(a, b):
    a, b = np.asarray(a), np.asarray(b)
    na, nb = np.linalg.norm(a), np.linalg.norm(b)
    return float(a @ b / (na * nb)) if na and nb else 0.0


_repo_cache = {}
def _read_origin(cfg_path):
    """Read the git remote 'origin' url from .git/config by PARSING THE FILE (never runs git)."""
    try:
        lines = open(cfg_path, encoding="utf-8", errors="replace").read().splitlines()
    except Exception:
        return None
    sect, urls = None, {}
    for ln in lines:
        s = ln.strip()
        if s.startswith("[") and s.endswith("]"):
            sect = s[1:-1].strip()
        elif sect and sect.startswith("remote ") and s.lower().startswith("url") and "=" in s:
            name = sect.split('"')[1] if '"' in sect else sect
            urls[name] = s.split("=", 1)[1].strip()
    return urls.get("origin") or (next(iter(urls.values())) if urls else None)


def repo_identity(path):
    """Identity of the repo a path belongs to: git origin url (read from .git/config, NO exec)
    if it's a git repo, else the repo root, else the standard directory path. T0.4 / spec 2.1."""
    if not path:
        return None
    path = os.path.abspath(path)
    if path in _repo_cache:
        return _repo_cache[path]
    d, ident = path, path
    while True:
        gitdir = os.path.join(d, ".git")
        cfg = os.path.join(gitdir, "config")
        if os.path.isfile(cfg):
            ident = _read_origin(cfg) or d        # origin url, else repo-root path
            break
        if os.path.isdir(gitdir):
            ident = d; break
        parent = os.path.dirname(d)
        if parent == d:
            ident = path; break                   # no git anywhere -> standard cwd path
        d = parent
    _repo_cache[path] = ident
    return ident


def group_key(cmd):
    """Phase 1 normalizer: collapse variants under one intent. base program + subcommand,
    args/paths/quotes stripped. e.g. scp -r "app" root@h:/p -> 'scp';
    alembic upgrade head -> 'alembic upgrade'; git commit -m "x" -> 'git commit'.
    A token counts as a subcommand only if it's a bare word (not a filename/path/arg),
    so `scp index.html ...` groups under 'scp', not 'scp index.html'."""
    prog, sub = parse_cmd(cmd)
    if not prog:
        return cmd.strip()[:40]
    if sub and re.match(r'^[a-z][a-z0-9_-]*$', sub):   # bare word -> real subcommand
        return f"{prog} {sub}"
    return prog


def search(query, k=5, worked_only=False, here=None, grouped=True):
    db = sqlite3.connect(DB_PATH)
    qv = np.asarray(embed(query))
    # all description vectors per command (gen + spec) -> max-blended
    desc_vecs = defaultdict(list)
    for cid, vec in db.execute("SELECT command_id, vec FROM command_desc_vec"):
        desc_vecs[cid].append(unpack(vec))
    here_id = repo_identity(here) if here else None   # --here matches by repo identity (file-based)
    conds = []
    if worked_only:                    # --worked: success pool only (exit 0 actually seen)
        conds.append("c.success_count > 0")
    where = ("WHERE " + " AND ".join(conds)) if conds else ""
    rows = db.execute(f"""
        SELECT c.id, c.cmd_text, c.description, c.last_used, c.run_count, cv.vec, c.cwd
        FROM commands c JOIN command_vec cv ON cv.command_id = c.id {where}
    """).fetchall()
    now = time.time()
    scored = []
    for cid, cmd, desc, last_used, rc, rawv, cwd in rows:
        if cmd.strip().startswith("#"):          # a comment is not a runnable command
            continue
        if here_id is not None and (not cwd or repo_identity(cwd) != here_id):
            continue                             # --here: skip commands from other repos
        sim_raw = _cos(qv, unpack(rawv))
        sim_desc = max((_cos(qv, d) for d in desc_vecs.get(cid, [])), default=-1.0)
        sim = max(sim_raw, sim_desc)
        won = "desc" if sim_desc > sim_raw else "raw"
        # Fix B: semantics dominate. Frequency + recency are bounded tiebreakers (<=0.05 total),
        # so a high run_count can never bury a clearly-better semantic match.
        freq = 0.03 * (rc / (rc + 20.0))                                  # in [0, 0.03)
        recency = 0.02 / (1.0 + max(0, (now - (last_used or now)) / 86400.0))  # in [0, 0.02]
        score = sim + freq + recency
        scored.append((score, sim, sim_raw, sim_desc, won, cmd, desc, rc, cwd))
    scored.sort(reverse=True)
    best_sim = scored[0][1] if scored else 0.0

    # CONFIDENCE = decent semantic sim AND some top result literally shares a content word
    # with the query. Literal-overlap is what separates real intent from hot-floor noise
    # (gibberish has none); seed-independent, so it generalizes. See margin_diag.py.
    qtokens = [w for w in re.findall(r'[a-z0-9]+', query.lower()) if len(w) > 2 and w not in STOP]
    top = scored[:max(k, 5)]
    has_lit = any(any(w in (it[5] + " " + (it[6] or "")).lower() for w in qtokens) for it in top)
    confident = bool(scored) and best_sim >= WEAK_SIM and (not qtokens or has_lit)

    # MANUAL FALLBACK (user rule): not confident -> simple literal substring search over the
    # REAL command history. Still pure retrieval - invents nothing.
    if not confident:
        words = [w.lower() for w in query.split() if len(w) > 1]
        lit = []
        for cid, cmd, desc, last_used, rc, rawv, cwd in rows:
            if cmd.strip().startswith("#"):
                continue
            low = cmd.lower()
            n = sum(1 for w in words if w in low)
            if n:
                lit.append((n, rc, cmd))
        lit.sort(reverse=True)
        return ("manual", lit[:k])

    if not grouped:
        return ("semantic", [(it, group_key(it[5]), 1) for it in scored[:k]])

    # PHASE 1: collapse variants under one group_key; best-scoring variant represents it.
    sizes = Counter(group_key(it[5]) for it in scored)
    out, seen = [], set()
    for it in scored:
        gk = group_key(it[5])
        if gk in seen:
            continue
        seen.add(gk)
        out.append((it, gk, sizes[gk]))
        if len(out) >= k:
            break
    return ("grouped", out)


def did_you_mean(failed_cmd, k=3, worked_only=True, here=None):
    """Phase 5: on a FAILED command, surface similar commands from the user's OWN successes.
    Dual signal: typo-distance (catches gti->git) + semantic (catches wrong-but-meant-right),
    success-pool only, scoped to repo. Pure retrieval - suggests real past commands, never invents."""
    db = sqlite3.connect(DB_PATH)
    qv = np.asarray(embed(failed_cmd))
    here_id = repo_identity(here) if here else None
    where = "WHERE c.success_count > 0" if worked_only else ""
    rows = db.execute(f"""
        SELECT c.id, c.cmd_text, cv.vec, c.cwd, c.run_count
        FROM commands c JOIN command_vec cv ON cv.command_id = c.id {where}
    """).fetchall()
    seen, scored = set(), []
    for cid, cmd, rawv, cwd, rc in rows:
        if cmd.strip().startswith("#") or cmd == failed_cmd:
            continue
        if here_id is not None and (not cwd or repo_identity(cwd) != here_id):
            continue
        gk = group_key(cmd)
        if gk in seen:
            continue
        seen.add(gk)
        typo = 1.0 - norm_edit(failed_cmd, cmd)      # fat-finger closeness
        sem = _cos(qv, unpack(rawv))                  # intent closeness
        score = 0.55 * typo + 0.45 * sem              # typo usually wins, semantic fills gaps
        scored.append((score, typo, sem, cmd))
    scored.sort(reverse=True)
    return scored[:k]


def print_results(mode, items):
    if mode == "manual":
        print("  (semantic match weak -> literal history search)")
        if not items:
            print("  (no command in your history matches those words)")
        for n, rc, cmd in items:
            print(f"\n  {cmd}\n    matched {n} term(s)  runs={rc}")
        return
    for it, gk, size in items:
        score, sim, sim_raw, sim_desc, won, cmd, desc, rc, cwd = it
        sd = f"{sim_desc:.3f}" if sim_desc >= 0 else "  -  "
        vlabel = f"   (+{size-1} more variants)" if size > 1 else ""
        where = f"  @ {cwd}" if cwd else ""
        print(f"\n  {cmd}{vlabel}")
        print(f"    sim={sim:.3f} [{won}]  runs={rc}   group: {gk}{where}")
        if desc:
            print(f"    desc: {desc}")


def main():
    if len(sys.argv) < 2:
        print("usage: reman_enrich.py [enrich | search <query>]"); return
    if sys.argv[1] == "enrich":
        enrich()
    elif sys.argv[1] == "search":
        raw = sys.argv[2:]
        worked, here, qparts, i = False, None, [], 0
        while i < len(raw):
            a = raw[i]
            if a == "--worked":
                worked = True
            elif a == "--here":
                if i + 1 < len(raw) and not raw[i + 1].startswith("--"):
                    here = raw[i + 1]; i += 1
                else:
                    here = os.getcwd()
            else:
                qparts.append(a)
            i += 1
        mode, items = search(" ".join(qparts), k=5, worked_only=worked, here=here)
        print_results(mode, items)
    elif sys.argv[1] == "didyoumean":
        raw = sys.argv[2:]
        worked, here, qparts, i = True, None, [], 0
        while i < len(raw):
            a = raw[i]
            if a == "--all":            # include non-success (seed has empty success pool)
                worked = False
            elif a == "--here":
                if i + 1 < len(raw) and not raw[i + 1].startswith("--"):
                    here = raw[i + 1]; i += 1
                else:
                    here = os.getcwd()
            else:
                qparts.append(a)
            i += 1
        failed = " ".join(qparts)
        print(f"  command failed: {failed}\n  did you mean one of these that worked?")
        for score, typo, sem, cmd in did_you_mean(failed, k=3, worked_only=worked, here=here):
            print(f"\n  {cmd}\n    score={score:.3f}  (typo={typo:.3f}  intent={sem:.3f})")


if __name__ == "__main__":
    main()
