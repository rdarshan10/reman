"""Search quality against a noisy, realistic history in a sandbox daemon (own db, own port; your
real history is never touched):

  * FOUND: a description of a command that IS in the history must rank it first, marked close;
  * ABSENT: a description of something never run must come back with nothing marked close.

The history mixes everyday human commands with ~250 long agent-style scripts (heredocs, grep
pipelines), because that noise is what makes a weak match look strong. The website's examples
come from FOUND, so this suite is what keeps them true.

  python e2e/search_quality.py            # pass/fail
  python e2e/search_quality.py --diag     # also print every query's similarities
  python e2e/search_quality.py --real-db  # against a COPY of your own history: ABSENT reported
  python e2e/search_quality.py --describe # the description each everyday command gets
  python e2e/search_quality.py --dump f   # write each top result's evidence (to tune search.rs)
"""
import json, os, random, shutil, socket, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
EXE = os.environ.get("REMAN_EXE_UNDER_TEST") or os.path.join(HERE, "..", "target", "release", "reman.exe" if os.name == "nt" else "reman")
PORT = 8797
DIAG = "--diag" in sys.argv
REAL = "--real-db" in sys.argv
API, WEB, HOME = "C:\\work\\api", "C:\\work\\web", "C:\\Users\\dev"

# (command, folder, runs)
EVERYDAY = [
    ("git status", API, 30), ("git log --oneline -20", API, 8), ("git diff --staged", API, 6), ("git add -A", API, 12),
    ('git commit -m "wip"', API, 10), ("git pull", API, 15), ("git checkout main", API, 9), ("git checkout -b feat/search", API, 2),
    ("git stash", API, 4), ("git stash pop", API, 4), ("git reset --soft HEAD~1", API, 5), ("git reset --hard HEAD", API, 1),
    ("git branch --merged main | grep -v main | xargs git branch -d", API, 3), ("git blame -L 40,80 app/search.py", API, 2),
    ("git log -p -S retry_policy", API, 2), ("git rebase -i HEAD~3", API, 2), ("git cherry-pick 4f2a9c1", API, 1),
    ("docker ps", API, 25), ("docker compose up -d", API, 20), ("docker compose down -v", API, 14), ("docker compose logs -f api", API, 11),
    ("docker compose build --no-cache", API, 4), ("docker system prune -af --volumes", API, 3), ("docker compose exec db psql -U postgres", API, 6),
    ("docker compose exec api bash", API, 5), ("docker images", API, 4),
    ("alembic upgrade head", API, 18), ('alembic revision --autogenerate -m "add users"', API, 3), ("alembic downgrade -1", API, 2),
    ("pytest -x -q", API, 22), ("pytest -k search -vv", API, 5), ("ruff check --fix .", API, 7), ("uvicorn app.main:app --reload", API, 16),
    ("pip install -r requirements.txt", API, 5), ("python -m venv .venv", API, 1), (".venv\\Scripts\\activate", API, 12),
    ("npm run dev", WEB, 30), ("npm install", WEB, 10), ("npm run build", WEB, 9), ("npm test", WEB, 8), ("npx prisma migrate dev", WEB, 4),
    ("npm outdated", WEB, 2), ("npx kill-port 3000", WEB, 3), ("rm -rf node_modules package-lock.json", WEB, 2),
    ("netstat -ano | findstr :3000", WEB, 3), ("taskkill /PID 18244 /F", WEB, 2), ("du -sh * | sort -rh | head -20", HOME, 3),
    ("df -h", HOME, 4), ("ssh deploy@staging.internal", HOME, 7), ("scp dump.sql deploy@staging.internal:/tmp", HOME, 2),
    ("curl -s localhost:8000/health | jq", API, 9), ("kubectl get pods -n api", HOME, 6), ("kubectl logs -f deploy/api -n api", HOME, 4),
    ("kubectl rollout restart deploy/api -n api", HOME, 2), ("code .", API, 10), ("ls -la", HOME, 20), ("cd ..", HOME, 20),
    ("openssl x509 -in cert.pem -noout -dates", HOME, 2), ("tar -czf backup.tgz data/", HOME, 2), ('find . -name "*.log" -mtime +7 -delete', HOME, 2),
    ("ipconfig /flushdns", HOME, 2), ("Get-ChildItem -Recurse -Filter *.env", HOME, 1), ("python scripts/seed_db.py --users 50", API, 3),
    ("make lint", API, 4), ("go test ./...", HOME, 3), ("cargo build --release", HOME, 5), ("terraform plan -out plan.tfplan", HOME, 2),
]

# description -> the command(s) that count as right
FOUND = [
    ("free up docker disk space", ["docker system prune -af --volumes"]),
    ("kill whatever is on port 3000", ["npx kill-port 3000"]),
    ("biggest folders here", ["du -sh * | sort -rh | head -20"]),
    ("delete merged branches", ["git branch --merged main | grep -v main | xargs git branch -d"]),
    ("when does the certificate expire", ["openssl x509 -in cert.pem -noout -dates"]),
    ("find where retry_policy was added", ["git log -p -S retry_policy"]),
    ("restart the api in kubernetes", ["kubectl rollout restart deploy/api -n api"]),
    ("dcoker ps", ["docker ps"]),
    ("tear down containers", ["docker compose down -v"]),
    ("undo my last commit", ["git reset --soft HEAD~1"]),
    ("rebuild images without cache", ["docker compose build --no-cache"]),
    ("run database migrations", ["alembic upgrade head"]),
    ("create a new migration", ['alembic revision --autogenerate -m "add users"']),
    ("follow the api logs", ["docker compose logs -f api", "kubectl logs -f deploy/api -n api"]),
    ("run the tests", ["pytest -x -q", "npm test"]),
    ("stash my changes", ["git stash"]),
    ("see what is staged", ["git diff --staged"]),
    ("log into the staging server", ["ssh deploy@staging.internal"]),
    ("list running pods", ["kubectl get pods -n api"]),
    ("lint and fix the python code", ["ruff check --fix .", "make lint"]),
    ("switch back to main", ["git checkout main"]),
    ("check for outdated packages", ["npm outdated"]),
    ("start the frontend dev server", ["npm run dev"]),
    ("clear the dns cache", ["ipconfig /flushdns"]),
    ("back up the data folder", ["tar -czf backup.tgz data/"]),
    ("copy the dump to staging", ["scp dump.sql deploy@staging.internal:/tmp"]),
]

# known limits: reported on every run, not asserted. Too little in the command or its description
# links them to the words; improving these is welcome, claiming them is not.
LIMITS = [
    ("open a database shell", ["docker compose exec db psql -U postgres"]),
    ("start the backend", ["uvicorn app.main:app --reload", "docker compose up -d"]),
    ("check the api is healthy", ["curl -s localhost:8000/health | jq"]),
    ("fill the database with test users", ["python scripts/seed_db.py --users 50"]),
    # found first, but its evidence (0.740, few rare words) sits where junk does too
    ("clean old log files", ['find . -name "*.log" -mtime +7 -delete']),
]

# never run in either history
ABSENT = [
    "flash firmware to the arduino", "compile the latex paper", "convert a video to a gif", "rotate the aws access keys",
    "resize every image in this folder", "mount the usb drive", "schedule a nightly cron job", "check the laptop battery health",
    "send an email from the terminal", "encrypt a folder with a password", "update homebrew packages", "clean the conda cache",
    "train the model on the gpu", "record the screen", "translate this file to french", "generate a qr code",
]
# absent from a real history too, as far as a stranger can tell; checked only with --real-db
ABSENT_REAL = ["who changed these lines"]

WORDS = ["netWorthTiers", "cardOutline", "userSession", "retryPolicy", "invoiceTotal", "authToken", "featureFlags", "rateLimiter",
         "searchIndex", "billingCycle", "profileCard", "themeToggle", "webhookQueue", "cacheKey", "exportCsv", "tenantId"]
PATHS = ["src/components/ProfileCard.tsx", "utils/__tests__/netWorthTiers.test.ts", "app/services/billing.py", "app/api/routes.py",
         "src/hooks/useSession.ts", "tests/test_search.py", "src/lib/featureFlags.ts", "app/models/user.py", "README.md", "package.json"]


def agent_noise(n, rnd):
    """Long, one-off exploration and edit scripts, the way coding agents run them."""
    out = []
    for _ in range(n):
        w, w2, p, p2 = rnd.choice(WORDS), rnd.choice(WORDS), rnd.choice(PATHS), rnd.choice(PATHS)
        t = rnd.randrange(9)
        if t == 0:
            c = f'grep -n "{w}" -A 4 {p}'
        elif t == 1:
            c = (f"python - <<'PY'\nimport io\np='{p}'\ns=io.open(p,encoding='utf-8').read()\ns=s.replace(\"{w}\",\"{w2}\")\n"
                 f"io.open(p,'w',encoding='utf-8').write(s)\nprint('{w} renamed to {w2}')\nPY\nnpx tsc --noEmit 2>&1 | grep -v node_modules || echo CLEAN")
        elif t == 2:
            c = f'git status -sb | head -3; echo "=== {w} ==="; git log --oneline -5 -- {p}'
        elif t == 3:
            c = f"sed -n '{rnd.randrange(1, 200)},{rnd.randrange(200, 400)}p' {p}"
        elif t == 4:
            c = f'rg -n "{w}|{w2}" src app --glob "*.ts" | head -40'
        elif t == 5:
            c = f"npx jest {p} 2>&1 | grep -E \"^Tests:|^Test Suites:|{w}\""
        elif t == 6:
            c = f"ls {p.rsplit('/', 1)[0]}; echo ---; cat {p2} | head -60"
        elif t == 7:
            c = f"find . -name '*.ts' -not -path './node_modules/*' | xargs grep -l \"{w}\" | head"
        else:
            c = f"node -e \"const m=require('./{p}'); console.log(Object.keys(m).filter(k=>k.includes('{w[:4]}')))\""
        out.append(c)
    return out


def call(obj, timeout=60):
    with socket.create_connection(("127.0.0.1", PORT), timeout=timeout) as s:
        s.sendall((json.dumps(obj) + "\n").encode())
        buf = b""
        while not buf.endswith(b"\n"):
            chunk = s.recv(1 << 20)
            if not chunk:
                break
            buf += chunk
    return json.loads(buf)


def wait_up(env, proc):
    for _ in range(240):
        if proc.poll() is not None:
            sys.exit(f"daemon exited ({proc.returncode})")
        try:
            call({"op": "ping"}, timeout=2)
            break
        except OSError:
            time.sleep(0.5)
    else:
        sys.exit("daemon did not start")
    # an older db is re-described once in the background: test the finished result
    for _ in range(1200):
        if call({"op": "ping"}).get("descriptions_current", True):
            return
        time.sleep(0.5)
    sys.exit("descriptions never became current")


def ingest(cmd, cwd, actor="human", exit=0, session="s1"):
    call({"op": "ingest", "command": cmd, "exit": exit, "cwd": cwd, "session": session, "actor": actor, "duration_ms": 900})


def main():
    tmp = tempfile.mkdtemp(prefix="reman-quality-")
    env = dict(os.environ, REMAN_DB=os.path.join(tmp, "reman.db"), REMAN_PORT=str(PORT), REMAN_SPOOL=os.path.join(tmp, "spool.jsonl"))
    if REAL:
        shutil.copy(os.path.join(os.path.expanduser("~"), ".reman", "reman.db"), env["REMAN_DB"])
    proc = subprocess.Popen([EXE, "daemon", "--port", str(PORT)], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        wait_up(env, proc)
        if not REAL:
            rnd = random.Random(7)
            rows = [(c, d, "human") for c, d, n in EVERYDAY for _ in range(n)] + [(c, rnd.choice([API, WEB]), "agent:claude-code") for c in agent_noise(250, rnd)]
            rnd.shuffle(rows)
            for i, (c, d, a) in enumerate(rows):
                ingest(c, d, actor=a, session=f"s{i % 9}")
        if "--describe" in sys.argv:
            for c, _, _ in EVERYDAY:
                print(f"  {c[:48]:<48}  {call({'op': 'describe', 'command': c})['description']}")
            return 0
        return run_checks()
    finally:
        try:
            call({"op": "shutdown"}, timeout=5)
        except OSError:
            pass
        proc.kill()
        proc.wait()
        shutil.rmtree(tmp, ignore_errors=True)


DUMP = sys.argv[sys.argv.index("--dump") + 1] if "--dump" in sys.argv else None
dumped = []


def search(q, k=60):
    r = call({"op": "search", "query": q, "k": k, "group": False, "scope": "all"})
    if DUMP:
        # the evidence behind each top result, to calibrate search.rs's close rule on
        for x in r["results"][:8]:
            dumped.append({"query": q, "command": x["command"], "sim": x.get("similarity", 0), "words": x.get("words", 0),
                           "described": bool(x.get("description")), "close": x.get("close"),
                           "oneoff": x.get("actor", "").startswith("agent") and x.get("runs", 0) <= 1})
    return r


def show(q, r, expect=None):
    res = r["results"]
    sims = sorted((x.get("similarity") or 0 for x in res), reverse=True)
    at = lambda i: sims[min(i, len(sims) - 1)] if sims else 0
    print(f"\n  {q!r}  mode={r['mode']} confident={r['confident']}  top={at(0):.3f} #2={at(1):.3f} #10={at(9):.3f} #30={at(29):.3f}")
    for x in res[:3]:
        mark = "*" if expect and x["command"] in expect else " "
        print(f"   {mark} {x.get('similarity', 0):.3f} words={x.get('words', 0):.2f} close={x.get('close')}  {x['command'][:80]!r}")


def run_checks():
    ok = []

    def check(name, cond, detail=""):
        ok.append(cond)
        print(f"  {'ok  ' if cond else 'FAIL'} {name}" + (f"  ({detail})" if detail else ""))

    if not REAL:
        print("FOUND: the command described ranks first, marked close")
        for q, expect in FOUND:
            r = search(q)
            if DIAG:
                show(q, r, expect)
            top = r["results"][0] if r["results"] else {}
            check(q, top.get("command") in expect and top.get("close") is True,
                  f"got {top.get('command', '-')!r}, close={top.get('close')}")
        print("\nknown limits (not asserted)")
        for q, expect in LIMITS:
            r = search(q)
            top = r["results"][0] if r["results"] else {}
            hit = top.get("command") in expect
            verdict = ("found, close" if top.get("close") else "found, not close") if hit else "missed"
            print(f"  --   {q}  ({verdict}: {top.get('command', '-')!r})")
    print("\nABSENT: nothing marked close" + (" (a real history may hold something related: reported, not asserted)" if REAL else ""))
    for q in ABSENT + (ABSENT_REAL if REAL else []):
        r = search(q)
        if DIAG:
            show(q, r)
        close = [x["command"] for x in r["results"] if x.get("close")]
        if REAL and q not in ABSENT_REAL:
            print(f"  --   {q}" + (f"  (close: {close[0][:70]!r})" if close else ""))
            continue
        check(q, not close, f"close: {close[0][:70]!r}" if close else "")
    if DUMP:
        with open(DUMP, "w", encoding="utf-8") as f:
            json.dump(dumped, f, indent=0)
    passed = sum(ok)
    print(f"\n{passed}/{len(ok)} passed")
    return 0 if passed == len(ok) else 1


if __name__ == "__main__":
    sys.exit(main())
