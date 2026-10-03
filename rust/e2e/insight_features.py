"""The folder insights, end to end on a sandbox daemon (own db, own port; your history is never
touched):

  * last time here: a shell arriving in a folder you left days ago gets one line of what you did
    there, once per session, and never for a folder you were just in;
  * what broke it: a command that kept working here fails, and the reply says how often it worked
    and what ran here since (`reman why` tells the whole story);
  * runbook: how the project is run, by task, from commands a person ran that worked, for
    `reman runbook` and the reman_runbook agent tool.

  python e2e/insight_features.py
"""
import json, os, shutil, socket, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
EXE = os.environ.get("REMAN_EXE_UNDER_TEST") or os.path.join(HERE, "..", "target", "release", "reman.exe" if os.name == "nt" else "reman")
HOOK = os.path.join(os.path.dirname(EXE), "reman-hook.exe" if os.name == "nt" else "reman-hook")
PORT = 8794
WEB, API, FRESH = r"C:\work\web", r"C:\work\api", r"C:\work\fresh"
DAY = 86400
NOW = int(time.time())

results = []


def check(name, ok, detail=""):
    results.append(ok)
    print(f"  {'ok  ' if ok else 'FAIL'} {name}" + (f"  ({detail})" if detail and not ok else ""))


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


def ingest(cmd, cwd, ts, exit=0, actor="human", session="s-old"):
    return call({"op": "ingest", "command": cmd, "exit": exit, "cwd": cwd, "session": session, "actor": actor, "ts": ts, "duration_ms": 800})


def cli(*args, cwd=None):
    env = dict(os.environ, REMAN_PORT=str(PORT))
    p = subprocess.run([EXE, *args], cwd=cwd, env=env, capture_output=True, text=True, encoding="utf-8")
    return p.stdout + p.stderr


def main():
    tmp = tempfile.mkdtemp(prefix="reman-insight-")
    env = dict(os.environ, REMAN_DB=os.path.join(tmp, "reman.db"), REMAN_PORT=str(PORT), REMAN_SPOOL=os.path.join(tmp, "spool.jsonl"),
               REMAN_NOTIFY_LOG=os.path.join(tmp, "notify.log"), REMAN_CONFIG=os.path.join(tmp, "config.json"))
    proc = subprocess.Popen([EXE, "daemon", "--port", str(PORT)], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(240):
            try:
                call({"op": "ping"}, timeout=2)
                break
            except OSError:
                time.sleep(0.5)
        run(tmp)
    finally:
        try:
            call({"op": "shutdown"}, timeout=5)
        except OSError:
            pass
        proc.kill()
        proc.wait()
        shutil.rmtree(tmp, ignore_errors=True)
    passed = sum(results)
    print(f"\nINSIGHT RESULT: {'PASS' if passed == len(results) else 'FAIL'} ({passed}/{len(results)})")
    return 0 if passed == len(results) else 1


def run(tmp):
    # --- a history --------------------------------------------------------------------------
    t = NOW - 20 * DAY
    # web: the usual day, repeated; `npm test` keeps passing
    for d in range(5):
        base = t + d * DAY
        for i, c in enumerate(["git pull", "npm install", "npx prisma migrate dev", "npm run dev", "npm test", "npm run lint", "npm run build"]):
            ingest(c, WEB, base + i * 60)
    ingest("npm run test:e2e", WEB, t + 5 * DAY, exit=1)  # only ever failed: never in the runbook
    ingest("cd web; grep -rn TODO src | head", WEB, t + 5 * DAY + 10, actor="agent:claude-code")  # agent only
    # api: the last visit, 3 days ago
    last = NOW - 3 * DAY
    for i, c in enumerate(["git status", "docker compose up -d", "alembic upgrade head", "uvicorn app.main:app --reload"]):
        ingest(c, API, last - 600 + i * 60)
    # fresh: you were just there
    for i, c in enumerate(["cargo build", "cargo test"]):
        ingest(c, FRESH, NOW - 1800 + i * 60)

    # --- last time here ---------------------------------------------------------------------
    print("last time here")
    r = call({"op": "welcome", "cwd": API, "session": "s-new"})
    line = r.get("line") or ""
    print("   ", line)
    check("a folder left days ago gets a line", line.startswith("last time here (3 days ago):"), line)
    check("it lists what you did there, in order", "docker compose up -d → alembic upgrade head → uvicorn app.main:app --reload" in line, line)
    check("looking around is left out (git status)", "git status" not in line, line)
    check("once per session and folder", call({"op": "welcome", "cwd": API, "session": "s-new"}).get("line") is None)
    check("a new session gets it again", bool(call({"op": "welcome", "cwd": API, "session": "s-other"}).get("line")))
    check("never for a folder you were just in", call({"op": "welcome", "cwd": FRESH, "session": "s-new"}).get("line") is None)
    check("nothing for a folder with no history", call({"op": "welcome", "cwd": r"C:\nowhere", "session": "s-new"}).get("line") is None)
    out = subprocess.run([HOOK, "welcome", "--cwd", API, "--session", "s-fish"], env=dict(os.environ, REMAN_PORT=str(PORT)), capture_output=True, text=True, encoding="utf-8")
    check("fish's reman-hook welcome prints it", "last time here" in out.stderr, out.stderr + out.stdout)
    out = cli("here", cwd=tmp)
    check("`reman here` with no history here says so", "nothing you did" in out, out)

    # --- what broke it ----------------------------------------------------------------------
    print("what broke it")
    tb = t + 5 * DAY + 3600
    ingest("git pull", WEB, tb)
    ingest("npm install left-pad@2", WEB, tb + 60)
    ingest("ls", WEB, tb + 90)
    r = ingest("npm test", WEB, tb + 120, exit=1, session="s-now")
    note = r.get("note") or ""
    print("   ", note)
    check("the failure reply says it used to work", "`npm test` worked here 5 times" in note, note)
    check("and what ran here since, changes first", "since then here: git pull → npm install left-pad@2" in note, note)
    check("looking around is left out (ls)", " ls" not in note, note)
    w = call({"op": "why", "cwd": WEB})
    check("why (no command) picks the last failure here", w.get("found") and w.get("command") == "npm test", w)
    check("why lists what ran in between", w.get("between", [])[:2] == ["git pull", "npm install left-pad@2"], w.get("between"))
    check("why's timeline ends with the failure", w.get("timeline") and w["timeline"][-1]["command"] == "npm test" and w["timeline"][-1]["exit"] == 1, w.get("timeline"))
    w = call({"op": "why", "cwd": WEB, "command": "npm run test:e2e"})
    check("why explains a command that never worked", not w.get("found") and "never worked" in w.get("reason", ""), w)
    r = ingest("npm run lint", WEB, tb + 200, exit=1)
    check("a second command breaking gets its own note", "worked here 5 times" in (r.get("note") or ""), r)
    r = ingest("npm run test:e2e", WEB, tb + 300, exit=1)
    check("no note for a command that never worked", "note" not in r, r)

    # --- agents: reman_check says it stopped working ---------------------------------------
    c = call({"op": "mcp", "tool": "reman_check", "args": {"command": "npm test", "cwd": WEB}, "allow_global": True})
    sw = c.get("stopped_working") or {}
    check("reman_check: stopped working, with what ran since", sw.get("worked_before") == 5 and "git pull" in sw.get("ran_here_since", []), c)

    # --- what broke it: what git says changed ----------------------------------------------
    # a real checkout on disk (just the files git keeps); runs read its branch and commit as they
    # arrive, so these happen now
    print("what broke it: git")
    repo = os.path.join(tmp, "gitapp")
    git = os.path.join(repo, ".git")
    os.makedirs(os.path.join(git, "refs", "heads", "feat"))
    os.makedirs(os.path.join(repo, "src"))
    sha_a, sha_b = "a" * 40, "b" * 40
    def head(ref):
        open(os.path.join(git, "HEAD"), "w").write(f"ref: refs/heads/{ref}\n")
    def commit(ref, sha):
        open(os.path.join(git, "refs", "heads", *ref.split("/")), "w").write(sha + "\n")
    commit("main", sha_a); commit("feat/x", sha_a); head("main")
    src = os.path.join(repo, "src")
    for k in range(3):
        ingest("cargo test", src, NOW - 300 + k, session="s-git")
    head("feat/x")
    r = ingest("cargo test", src, NOW - 200, exit=1, session="s-git")
    note = r.get("note") or ""
    print("   ", note)
    check("another branch: the note says it worked on main, you're on feat/x", "it worked on main, you're on feat/x now" in note, note)
    w = call({"op": "why", "cwd": src, "command": "cargo test"})
    g = w.get("git") or {}
    check("why: worked on main, fails on feat/x, and how to compare them",
          (g.get("worked_on") or {}).get("branch") == "main" and (g.get("fails_on") or {}).get("branch") == "feat/x" and "git diff main...feat/x" in (g.get("advice") or ""), w)
    check("why's timeline carries each run's branch", w.get("timeline") and w["timeline"][-1].get("branch") == "feat/x", w.get("timeline"))
    out = cli("why", "cargo", "test", cwd=src)
    check("`reman why` prints it", "It worked on branch main and fails on feat/x" in out, out)
    c = call({"op": "mcp", "tool": "reman_check", "args": {"command": "cargo test", "cwd": src}, "allow_global": True})
    check("reman_check tells the agent too", (c.get("stopped_working") or {}).get("git") == "it worked on main, you're on feat/x now" and "git diff main...feat/x" in c.get("advice", ""), c)
    # same branch, new commits
    head("main")
    for k in range(3):
        ingest("cargo build", src, NOW - 100 + k, session="s-git")
    commit("main", sha_b)
    r = ingest("cargo build", src, NOW - 50, exit=1, session="s-git")
    check("new commits: the note says the code changed", "the code changed since (aaaaaaa → bbbbbbb)" in (r.get("note") or ""), r)
    # same commit: the change isn't committed code
    for k in range(3):
        ingest("cargo clippy", src, NOW - 40 + k, session="s-git")
    r = ingest("cargo clippy", src, NOW - 30, exit=1, session="s-git")
    check("same commit: the note says so", "same commit as when it worked" in (r.get("note") or ""), r)
    # no checkout: no git clause at all
    check("no checkout, no git clause", "worked on" not in (call({"op": "why", "cwd": WEB}).get("git") or {}).get("change", ""))

    # --- typical run time ---------------------------------------------------------------------
    print("typical run time")
    for ms in (180_000, 192_000, 200_000):
        call({"op": "ingest", "command": "cargo build --release", "exit": 0, "cwd": API, "session": "s-time", "actor": "human", "ts": NOW - 900, "duration_ms": ms})
    c = call({"op": "mcp", "tool": "reman_check", "args": {"command": "cargo build --release", "cwd": API}, "allow_global": True})
    check("reman_check: how long it usually takes, and to allow for it", c.get("typical_duration") == "3m 12s" and "allow for that" in c.get("advice", ""), c)
    hits = call({"op": "search", "query": "cargo build --release", "cwd": API, "k": 3}).get("results", [])
    check("search items carry typical_ms (for the finder's card)", any(h["command"] == "cargo build --release" and h.get("typical_ms") == 192_000 for h in hits), hits[:1])

    # --- time words in search ----------------------------------------------------------------
    print("time words")
    import datetime
    noon_yday = int((datetime.datetime.now().replace(hour=12, minute=0, second=0, microsecond=0) - datetime.timedelta(days=1)).timestamp())
    ingest("terraform apply -auto-approve", API, noon_yday, session="s-tw")
    ingest("terraform plan", API, NOW - 30 * DAY, session="s-tw")
    r = call({"op": "search", "query": "terraform yesterday", "cwd": API, "k": 5})
    got = [x["command"] for x in r.get("results", [])]
    check("`terraform yesterday`: only what ran yesterday", got == ["terraform apply -auto-approve"] and r.get("window") == "yesterday", r)
    r = call({"op": "search", "query": "terraform today", "cwd": API, "k": 5})
    check("`terraform today`: no terraform ran today", not any("terraform" in x["command"] for x in r.get("results", [])), r)
    r = call({"op": "search", "query": "yesterday", "cwd": API, "k": 40})
    check("`yesterday` alone browses yesterday", "terraform apply -auto-approve" in [x["command"] for x in r.get("results", [])] and "terraform plan" not in [x["command"] for x in r.get("results", [])], r.get("results", [])[:5])
    r = call({"op": "search", "query": "echo today is fine", "cwd": API, "k": 5})
    check("time words in the middle are text", "window" not in r, r)
    m = call({"op": "mcp", "tool": "reman_search", "args": {"intent": "terraform yesterday", "cwd": API}, "allow_global": True})
    cmds = [x.get("command") for x in (m if isinstance(m, list) else m.get("results") or m.get("value") or [])]
    check("agents' reman_search reads it too", cmds[:1] == ["terraform apply -auto-approve"], m)

    # --- reman yesterday ---------------------------------------------------------------------
    print("reman yesterday")
    shop = os.path.join(tmp, "projects", "web-shop")
    billing = os.path.join(tmp, "projects", "billing-api")
    for d in (shop, billing):
        os.makedirs(d)
    y = noon_yday
    ingest("docker compose up", shop, y, exit=1, session="s-day")
    ingest("docker compose up --build", shop, y + 60, session="s-day")
    ingest("npm test", shop, y + 120, session="s-day")
    ingest("git status", shop, y + 150, session="s-day")
    ingest("npm test", shop, y + 180, session="s-day")
    ingest("npm run lint", shop, y + 200, exit=1, session="s-day")
    ingest("npm run build", shop, y + 220, actor="agent:codex", session="s-day-agent")
    ingest("npm run build", shop, y + 240, exit=1, actor="agent:codex", session="s-day-agent")
    ingest("alembic upgrade head", billing, y + 3600, session="s-day2")
    r = call({"op": "day", "day": "yesterday"})
    p = next((x for x in r.get("projects", []) if x["name"] == "web-shop"), {})
    flow = [(f["command"], f["times"]) for f in p.get("flow", [])]
    check("yesterday: by project, repeats folded, looking around left out",
          flow == [("docker compose up --build", 1), ("npm test", 2)], p)
    fails = {f["command"]: f for f in p.get("failures", [])}
    check("...a failure with what fixed it", (fails.get("docker compose up") or {}).get("fixed_by") == "docker compose up --build", fails)
    check("...and one still failing", "npm run lint" in fails and not fails["npm run lint"].get("fixed_by") and not fails["npm run lint"].get("worked_later"), fails)
    check("...agents in one line", p.get("agents") == [{"agent": "codex", "runs": 2, "failed": 1}], p.get("agents"))
    check("...each project with its hours", bool(any(x["name"] == "billing-api" for x in r.get("projects", [])) and p.get("from") and p.get("to")), r.get("projects"))
    out = cli("yesterday")
    print("   " + out.replace("\n", "\n    ")[:900])
    check("`reman yesterday` prints it", "web-shop" in out and "docker compose up" in out and "(fixed)" in out and "still failing" in out and "codex ran 2 commands here (1 failed)" in out, out)
    check("`reman day` rejects a non-day", "isn't a day" in cli("day", "someday"))

    # --- reman goto --------------------------------------------------------------------------
    print("reman goto")
    g = call({"op": "goto", "query": "alembic"})
    check("goto: the folder where you ran it", [x["folder"] for x in g.get("results", [])][:1] == [billing] and "alembic" in (g["results"][0].get("because") or ""), g)
    g = call({"op": "goto", "query": "billing api"})
    check("goto: by the folder's own name", [x["folder"] for x in g.get("results", [])][:1] == [billing], g)
    g = call({"op": "goto", "query": "rebuild the containers"})
    check("goto: by meaning", [x["folder"] for x in g.get("results", [])][:1] == [shop], g)
    check("goto: a folder that doesn't exist is never offered", all(os.path.isdir(x["folder"]) for x in call({"op": "goto", "query": "npm"}).get("results", [])))
    env = dict(os.environ, REMAN_PORT=str(PORT))
    pr = subprocess.run([EXE, "goto", "alembic"], env=env, capture_output=True, text=True, encoding="utf-8")
    check("`reman goto alembic` prints the folder (for rcd) and why", pr.stdout.strip() == billing and "alembic upgrade head" in pr.stderr, pr.stdout + pr.stderr)

    # --- done alerts --------------------------------------------------------------------------
    print("done alerts")
    nlog = os.path.join(tmp, "notify.log")
    def alerts():
        time.sleep(0.5)
        return open(nlog, encoding="utf-8").read().splitlines() if os.path.exists(nlog) else []
    def run(cmd, ms, exit=0, actor="human", ts=None):
        call({"op": "ingest", "command": cmd, "exit": exit, "cwd": API, "session": "s-alert", "actor": actor, "ts": ts or int(time.time()), "duration_ms": ms})
    before = len(alerts())
    run("cargo build --release", 100_000)
    a = alerts()[before:]
    check("a long command of yours that finished: a notification, faster than usual", a == ["✓ cargo build --release\t1m 40s, faster than usual (3m 12s) · in api"], a)
    run("npm run e2e", 70_000, exit=1)
    check("...a failure says so", alerts()[-1:] == ["✗ npm run e2e\tfailed after 1m 10s · in api"], alerts()[-1:])
    n = len(alerts())
    run("ls", 2_000)
    run("cargo test", 90_000, actor="agent:gemini")
    run("cargo bench", 90_000, ts=int(time.time()) - 3600)
    check("not for a quick one, an agent's, or an old one arriving late", len(alerts()) == n, alerts()[n:])

    # --- search filters, delete, prune, stats for a period (ideas from Atuin) -----------------
    print("search filters, delete, prune, stats")
    for k in range(3):
        ingest("kubectl get pods -n web", API, NOW - 200 + k, session="s-atu")
    for k in range(2):
        ingest("kubectl apply -f bad.yaml", API, NOW - 150 + k, exit=1, actor="agent:claude-code", session="s-atu-agent")
    ingest("vault read secret/payments", API, NOW - 100, session="s-atu")
    names = lambda r: [x["command"] for x in r.get("results", [])]  # noqa: E731
    r = call({"op": "search", "query": "kubectl", "cwd": API, "k": 10, "status": "fail"})
    check("search --failed: only what only ever failed", "kubectl apply -f bad.yaml" in names(r) and "kubectl get pods -n web" not in names(r)
          and all(x["status"] == "fail" for x in r.get("results", [])), names(r))
    r = call({"op": "search", "query": "kubectl", "cwd": API, "k": 10, "actor": "agent"})
    check("search --by agents: only what agents ran", "kubectl apply -f bad.yaml" in names(r) and "kubectl get pods -n web" not in names(r)
          and all(x["actor"].startswith("agent") for x in r.get("results", [])), names(r))
    r = call({"op": "search", "query": "terraform", "cwd": API, "k": 10, "before": "last week"})
    check("search --before 'last week': what ran before it", names(r) == ["terraform plan"], names(r))
    r = call({"op": "search", "query": "terraform", "cwd": API, "k": 10, "after": "monday"})
    check("search --after monday: what ran since", "terraform plan" not in names(r), names(r))
    out = cli("search", "kubectl", "--failed", "--cwd", API, "--format", "{status}|{by}|{command}")
    check("search --format: one line per result, for scripts", out.strip().splitlines()[:1] == ["fail|claude-code|kubectl apply -f bad.yaml"], out)
    try:
        js = json.loads(cli("search", "kubectl", "--cwd", API, "--json"))
    except ValueError:
        js = None
    check("search --json", isinstance(js, list) and any(x.get("command") == "kubectl get pods -n web" for x in js), js)
    d = call({"op": "delete"})
    check("delete with nothing to match refuses", "error" in d, d)
    d = call({"op": "delete", "contains": "kubectl apply"})
    check("delete previews what it would remove, removing nothing", d.get("rows") == 1 and d.get("deleted") == 0 and call({"op": "detail", "command": "kubectl apply -f bad.yaml"}).get("found"), d)
    out = cli("delete", "kubectl", "apply", "--yes")
    check("reman delete --yes removes it", "Removed 1" in out and not call({"op": "detail", "command": "kubectl apply -f bad.yaml"}).get("found"), out)
    check("...and nothing else", call({"op": "detail", "command": "kubectl get pods -n web"}).get("found"))
    open(os.path.join(tmp, "config.json"), "w").write(json.dumps({"ignore_commands": ["^vault "]}))
    p = call({"op": "prune"})
    check("prune lists history an ignore rule now covers", [c["command"] for c in p.get("commands", [])] == ["vault read secret/payments"] and p.get("deleted") == 0, p)
    p = call({"op": "prune", "apply": True})
    check("prune removes it", p.get("deleted") == 1 and not call({"op": "detail", "command": "vault read secret/payments"}).get("found"), p)
    os.remove(os.path.join(tmp, "config.json"))
    st = call({"op": "stats", "period": "today"})
    tools = [t["tool"] for t in st.get("tools", [])]
    check("stats today: runs, and tools by subcommand", st.get("runs", 0) > 0 and "kubectl get" in tools, st)
    out = cli("stats", "week")
    check("`reman stats week` prints it", "most used:" in out and "most run:" in out, out)
    check("stats rejects a non-period", "isn't a period" in cli("stats", "someday"))

    # --- flaky ------------------------------------------------------------------------------
    print("flaky")
    flk = "pytest -q tests/test_queue.py"
    for k, e in enumerate([0, 1, 0, 0, 1, 0, 1, 0]):
        ingest(flk, API, NOW - 600 + k * 10, exit=e, session="s-flaky")
    r = ingest(flk, API, NOW - 500, exit=1, session="s-flaky")
    note = r.get("note") or ""
    print("   ", note)
    check("a flaky command's failure says it's flaky, not what broke", "is flaky here: it worked 5 of its last 9 runs," in note and "Try it again" in note, note)
    w = call({"op": "why", "cwd": API, "command": flk})
    check("why: nothing broke, it's flaky", w.get("flaky") is True and "Nothing broke" in w.get("reason", ""), w)
    c = call({"op": "mcp", "tool": "reman_check", "args": {"command": flk, "cwd": API}, "allow_global": True})
    check("reman_check tells the agent to retry before changing code", (c.get("flaky") or {}).get("of_last_runs") == 9 and "run it again once before changing code" in c.get("advice", ""), c)
    dev = "pytest -q tests/test_auth.py"
    for k, e in enumerate([1, 1, 0, 0, 0, 1]):
        r = ingest(dev, API, NOW - 400 + k * 10, exit=e, session="s-flaky")
    check("fail, fix, pass, break is not flaky", "flaky" not in (r.get("note") or ""), r)

    # --- an agent retrying a command that keeps failing the same way -------------------------
    # through the real Claude Code hook: from the third identical failure in a session, what it
    # prints goes into Claude's context
    print("agent retry loop")
    hook_env = dict(os.environ, REMAN_PORT=str(PORT))
    def claude_fails(cmd, err, session="s-loop", cwd=API):
        p = subprocess.run([HOOK, "claude"], input=json.dumps({"hook_event_name": "PostToolUseFailure", "tool_name": "Bash", "cwd": cwd,
                           "session_id": session, "tool_input": {"command": cmd}, "error": err}),
                           env=hook_env, capture_output=True, text=True, encoding="utf-8")
        return p.stdout.strip()
    err = "error[E0425]: cannot find value `cfg` in this scope"
    outs = [claude_fails("cargo build -p api", err) for _ in range(3)]
    check("the first two failures say nothing to the agent", outs[0] == "" and outs[1] == "", outs[:2])
    ctx = {}
    try:
        ctx = json.loads(outs[2]).get("hookSpecificOutput", {})
    except ValueError:
        pass
    print("   ", ctx.get("additionalContext"))
    check("the third, the same way: the hook tells Claude it's looping",
          ctx.get("hookEventName") == "PostToolUseFailure" and "has now failed 3 times in a row, the same way each time" in ctx.get("additionalContext", "")
          and "cannot find value" in ctx.get("additionalContext", ""), outs[2])
    check("a different error breaks the streak", claude_fails("cargo build -p api", "error: linker `link.exe` not found") == "")
    check("another session's failures don't count", claude_fails("cargo build -p api", err, session="s-other") == "")
    c = call({"op": "mcp", "tool": "reman_check", "args": {"command": "cargo build -p api", "cwd": API}, "allow_global": True})
    check("reman_check (no session): a different error in between breaks the run of failures", "retry_loop" not in c, c)
    for _ in range(3):
        claude_fails("cargo build -p api", err, session="s-loop2")
    c = call({"op": "mcp", "tool": "reman_check", "args": {"command": "cargo build -p api", "cwd": API}, "allow_global": True})
    check("reman_check: retried 4 times here the same way", (c.get("retry_loop") or {}).get("failed_the_same_way") == 4 and "change something first" in c.get("advice", ""), c)
    rep = call({"op": "agents", "days": 7})
    loops = [l for l in rep.get("loops", []) if l["command"] == "cargo build -p api"]
    claude = next((a for a in rep.get("actors", []) if a["actor"] == "claude-code"), {})
    check("reman agents: claude-code's runs and failures", claude.get("runs") == 8 and claude.get("failed") == 8, rep.get("actors"))
    check("reman agents: 3 in a row per session isn't a 4+ retry loop", loops == [], loops)
    for _ in range(2):
        claude_fails("npx tsc --noEmit", "src/a.ts(1,1): error TS2304: Cannot find name 'x'.", session="s-loop3")
        claude_fails("npx tsc --noEmit", "src/a.ts(1,1): error TS2304: Cannot find name 'x'.", session="s-loop3")
    rep = call({"op": "agents", "days": 7})
    tsc = [l for l in rep.get("loops", []) if l["command"] == "npx tsc --noEmit"]
    check("reman agents: a 4x same-error retry is listed", tsc and tsc[0]["times"] == 4 and tsc[0]["actor"] == "claude-code", rep.get("loops"))
    out = cli("agents")
    print("   " + out.replace("\n", "\n    "))
    check("`reman agents` prints it", "claude-code" in out and "npx tsc --noEmit" in out and "4x" in out, out)

    # --- what a fix changed ------------------------------------------------------------------
    print("what a fix changed")
    ingest("docker compose up", API, NOW - 20, exit=1, session="s-fix")
    r = ingest("docker compose up --build", API, NOW - 10, session="s-fix")
    r = ingest("docker compose up", API, NOW - 5, exit=1, session="s-fix2")
    sg = r.get("suggest") or {}
    check("the shell's suggestion says what the fix changes", sg.get("command") == "docker compose up --build" and sg.get("diff") == "adds --build", r)
    f = call({"op": "mcp", "tool": "reman_fixes", "args": {"failed_command": "docker compose up", "cwd": API}, "allow_global": True})
    first = (f if isinstance(f, list) else f.get("value") or f.get("result") or [{}])[0] if f else {}
    check("reman_fixes too (what_changed)", first.get("fixed_command") == "docker compose up --build" and first.get("what_changed") == "adds --build", f)

    # --- runbook ----------------------------------------------------------------------------
    print("runbook")
    rb = call({"op": "runbook", "cwd": WEB})
    secs = {s["title"]: [x["command"] for x in s["commands"]] for s in rb.get("sections", [])}
    print("   ", json.dumps(secs))
    check("set up: npm install", "npm install" in secs.get("Set up", []), secs)
    check("run: npm run dev", "npm run dev" in secs.get("Run", []), secs)
    check("test: npm test", "npm test" in secs.get("Test", []), secs)
    check("lint and format: npm run lint", "npm run lint" in secs.get("Lint and format", []), secs)
    check("build: npm run build", "npm run build" in secs.get("Build", []), secs)
    check("database: npx prisma migrate dev", "npx prisma migrate dev" in secs.get("Database", []), secs)
    everything = sum(secs.values(), [])
    check("never a command that only failed", "npm run test:e2e" not in everything, secs)
    check("never an agent's one-off", not any("grep" in c for c in everything), secs)
    check("never git pull (looking around / not a task)", "git pull" not in everything, secs)
    check("a one-off install is not how the project is set up", "npm install left-pad@2" not in everything, secs)
    check("the usual sequence is there", any(f["steps"][:2] == ["git pull", "npm install"] for f in rb.get("flows", [])), rb.get("flows"))
    m = call({"op": "mcp", "tool": "reman_runbook", "args": {"cwd": WEB}, "allow_global": True})
    check("reman_runbook gives agents the same, never generated", m.get("found") and m.get("generated") is False and len(m.get("sections", [])) >= 5, m)
    out = cli("runbook", cwd=tmp)
    check("`reman runbook` with no history here says so", "doesn't know" in out, out)

    # --- runbook: a repo with three apps, much of it run by agents -----------------------------
    print("runbook: three apps in one repo, agents' wrapped commands")
    mono = os.path.join(tmp, "mono")
    web, api, svc = os.path.join(mono, "web"), os.path.join(mono, "api"), os.path.join(mono, "svc")
    for d in (os.path.join(mono, ".git"), web, api, svc):
        os.makedirs(d, exist_ok=True)
    with open(os.path.join(web, "package.json"), "w") as f:
        json.dump({"scripts": {"start": "expo start", "test": "jest", "android": "expo run:android"}}, f)
    open(os.path.join(svc, "pytest.ini"), "w").close()
    tm = NOW - 10 * DAY
    n = iter(range(10_000))
    at = lambda: tm + next(n) * 400  # noqa: E731 - far enough apart to be no sequence
    agent = "agent:claude-code"

    def ingest_out(cmd, cwd, output, exit=0, actor=agent):
        return call({"op": "ingest", "command": cmd, "exit": exit, "cwd": cwd, "session": "s-old", "actor": actor, "ts": at(), "output": output})

    for _ in range(4):
        ingest("npx expo start --clear", web, at())
    for _ in range(2):
        ingest("npx expo start -c", web, at())
    ingest("npx expo start --c", web, at())
    ingest("npx expo start --c", web, at(), exit=1)
    for _ in range(3):
        ingest(f'cd {web} && npx jest --silent 2>&1 | grep -E "Tests:"', mono, at(), actor=agent)
    ingest("npx jest --silent", web, at())  # the one run whose result was seen
    for _ in range(3):
        ingest("cd web && npx jest src/a.test.ts 2>&1 | tail -5", mono, at(), actor=agent)
    # a type check that fails whenever its result is seen; `| wc -l` always exits 0
    for _ in range(4):
        ingest("npx tsc --noEmit 2>&1 | wc -l", web, at(), actor=agent)
    for _ in range(2):
        ingest("npx tsc --noEmit", web, at(), exit=2)
    # what the output said, when the exit status couldn't: a silent linter passing through tail,
    # the same with its default config named, and a build whose output shows it failing
    for _ in range(3):
        ingest_out("npx eslint . 2>&1 | tail -5", web, "")
    for _ in range(2):
        ingest("npx eslint . -c eslint.config.js", web, at())
    for _ in range(3):
        ingest_out("npm run build 2>&1 | tail -3", web, "npm ERR! code ELIFECYCLE\nnpm ERR! Failed at the build script")
    for _ in range(2):
        ingest(f'Set-Location {api}; . .\\env.ps1; cargo build --release 2>&1 | Select-String -Pattern "error"', mono, at(), actor=agent)
    ingest(". .\\env.ps1; cargo build --release 2>&1 | Select-Object -Last 5", api, at(), actor=agent)
    # `a && b` failing in a: b never ran
    for _ in range(2):
        ingest_out("cargo build && cargo test", api, "error[E0425]: cannot find value `x`\nerror: could not compile `api`", exit=101)
        ingest("cargo test", api, at())
    for _ in range(2):
        ingest("docker compose -f docker-compose.test.yml up --abort-on-container-exit", api, at())
    for _ in range(3):
        ingest("docker compose exec web alembic upgrade head", api, at())
    # svc: only ever one test file, and a pytest config
    for _ in range(2):
        ingest("pytest tests/test_x.py -q", svc, at())
    for _ in range(2):  # a server's lines, pasted here
        ingest("cd /opt/app-staging && rm -rf app.old", api, at())
        ingest("cd .\\web\\npm run dev", mono, at())  # two lines pasted into one
    # through the real agent hook: it reads what the agent saw and sends only the verdict
    hook_env = dict(os.environ, REMAN_PORT=str(PORT))
    for _ in range(3):
        payload = {"hook_event_name": "PostToolUse", "tool_name": "Bash", "cwd": mono, "session_id": "s-hook",
                   "tool_input": {"command": "cd web && npx vitest run 2>&1 | tail -3"},
                   "tool_response": {"stdout": " Test Files  2 passed (2)\n      Tests  10 passed (10)", "stderr": ""}}
        subprocess.run([HOOK, "claude"], input=json.dumps(payload), env=hook_env, capture_output=True, text=True, encoding="utf-8")
    time.sleep(1)

    rb = call({"op": "runbook", "cwd": web})
    got = {s["title"]: [(x["command"], x["dir"]) for x in s["commands"]] for s in rb.get("sections", [])}
    print("   ", json.dumps(got))
    every = sum(got.values(), [])
    by = lambda title, cmd: next((x for s in rb.get("sections", []) if s["title"] == title for x in s["commands"] if x["command"] == cmd), None)  # noqa: E731
    check("asked from web/: here is web", rb.get("here") == "web", rb.get("here"))
    check("run: one expo line, the one run most (variants folded)", [c for c in got.get("Run", []) if "expo" in c[0]] == [("npx expo start --clear", "web")], got.get("Run"))
    check("test: jest unwrapped from the agent's cd and grep, the general form", ("npx jest --silent", "web") in got.get("Test", []), got.get("Test"))
    check("test: the one-file variant is folded into it", not any("a.test.ts" in c[0] for c in every), got.get("Test"))
    jest = by("Test", "npx jest --silent") or {}
    check("test: a run whose result a pipe hid proves nothing (worked 1, 3 unseen)", (jest.get("runs"), jest.get("worked"), jest.get("failed"), jest.get("unseen")) == (4, 1, 0, 3), jest)
    check("a check that fails when seen is left out, however often a pipe said 0", not any("tsc" in c[0] for c in every), every)
    vitest = by("Test", "npx vitest run") or {}
    check("the hook read what the agent saw: vitest worked 3x, though tail hid it", (vitest.get("worked"), vitest.get("unseen"), vitest.get("dir")) == (3, 0, "web"), vitest)
    eslint = [c for c in got.get("Lint and format", []) if "eslint" in c[0]]
    check("a silent linter's empty output through tail is a pass", (by("Lint and format", "npx eslint .") or {}).get("worked") == 3, eslint)
    check("naming the default config is the same step (one eslint line)", len(eslint) == 1, eslint)
    check("a build whose output shows it failing is left out", not any("npm run build" in c[0] for c in every), every)
    ct = by("Test", "cargo test") or {}
    check("`cargo build && cargo test` failing in the build: the test never ran", (ct.get("runs"), ct.get("failed")) == (2, 0), ct)
    check("a compose file for tests is a test run", ("docker compose -f docker-compose.test.yml up --abort-on-container-exit", "api") in got.get("Test", []), got.get("Test"))
    one = by("Test", "pytest tests/test_x.py -q") or {}
    check("a one-file test run says so", one.get("partial") is True, one)
    declared = by("Test", "pytest") or {}
    check("...and the whole suite comes from the project's pytest config, not run yet", (declared.get("dir"), declared.get("declared"), declared.get("runs")) == ("svc", "pytest.ini", 0), declared)
    check("a declared script is left out when the history has the task (npm test, npm start)", not any(c[0] in ("npm test", "npm start") for c in every), every)
    build = [x for s in rb.get("sections", []) if s["title"] == "Build" for x in s["commands"]]
    check("build: runs where the agent moved to, with the environment it needs",
          [(b["command"], b["dir"]) for b in build] == [(". .\\env.ps1; cargo build --release", "api")], build)
    check("build: every wrapper of it counts (3 runs)", build and build[0]["runs"] == 3, build)
    check("database: from api/", ("docker compose exec web alembic upgrade head", "api") in got.get("Database", []), got.get("Database"))
    check("never a server's folder or a pasted-together line", not any("/opt" in c[0] or "npm run dev" in c[0] or "rm -rf" in c[0] for c in every), every)
    check("never an output filter or a cd", not any(k in c[0] for c in every for k in ("grep", "Select-", "tail", "cd ", "Set-Location", "2>&1")), every)
    m = call({"op": "mcp", "tool": "reman_runbook", "args": {"cwd": web}, "allow_global": True})
    check("agents get the folder of each command too", any(x.get("dir") == "api" for s in m.get("sections", []) for x in s["commands"]), m)
    check("...and the declared ones", any(x.get("declared") == "pytest.ini" for s in m.get("sections", []) for x in s["commands"]), m)
    out = cli("runbook", "--static", cwd=web)
    print("   ", out.replace("\n", "\n    "))
    check("`reman runbook` says where a command runs when it's elsewhere", "cargo build --release` in `api/`" in out, out)
    check("... and not for the folder you're in", "`npx expo start --clear` ·" in out, out)
    check("it says what it didn't see", "`npx jest --silent` · worked 1x (+3 runs, result not seen)" in out, out)
    check("it says a command comes from a project file", "`pytest` in `svc/` · in pytest.ini, not run yet" in out, out)
    check("it says a test run covers some files only", "`pytest tests/test_x.py -q` in `svc/` (some files only)" in out, out)

    # --- agents' sessions: search and resume ----------------------------------------------------
    print("agent sessions")
    shop = os.path.join(tmp, "shop-front")
    os.makedirs(shop, exist_ok=True)
    s1, s2, s3 = "11111111-aaaa-4bbb-8ccc-000000000001", "22222222-aaaa-4bbb-8ccc-000000000002", "conv-cursor-9"
    for k, c in enumerate(["grep -rn migrate src | head", "cd api && alembic upgrade head", "cd api && alembic upgrade head", "pytest -q"]):
        ingest(c, shop, NOW - 2 * DAY + k * 60, exit=1 if k == 1 else 0, actor="agent:claude-code", session=s1)
    for k, c in enumerate(["npm ci", "npm run build", "sed -n 1,80p src/app.tsx"]):
        ingest(c, shop, NOW - 3600 + k * 60, actor="agent:codex", session=s2)
    ingest("npm run lint", shop, NOW - 1800, actor="agent:cursor", session=s3)
    ingest("echo bad", shop, NOW - 1700, actor="agent:claude-code", session="x; rm -rf ~")
    mine = {s1, s2, s3, "x; rm -rf ~"}
    r = call({"op": "sessions", "k": 50})
    ids = [x["session"] for x in r.get("results", []) if x["session"] in mine]
    check("sessions: newest first", ids == ["x; rm -rf ~", s3, s2, s1], ids)
    by = {x["session"]: x for x in r.get("results", [])}
    a = by.get(s1, {})
    check("...with its folder, runs and failures", (a.get("folder"), a.get("runs"), a.get("failed")) == (shop, 4, 1), a)
    check("...what it did, as what it ran (no cd), grep left out", [f["command"] for f in a.get("flow", [])][:2] == ["alembic upgrade head", "pytest -q"], a.get("flow"))
    check("...and how to resume it", (a.get("resume"), by.get(s2, {}).get("resume")) == (f"claude --resume {s1}", f"codex resume {s2}"), (a.get("resume"), by.get(s2, {}).get("resume")))
    check("an editor's chat can't be resumed from a terminal", by.get(s3, {}).get("resume") is None, by.get(s3))
    check("an id that isn't plainly one is never put on a command line", by.get("x; rm -rf ~", {}).get("resume") is None, by.get("x; rm -rf ~"))
    r = call({"op": "sessions", "query": "database migration"})
    check("by meaning: the session that migrated the database", [x["session"] for x in r.get("results", [])][:1] == [s1], [x["session"] for x in r.get("results", [])])
    check("...and what it ran that matched", "alembic upgrade head" in (r.get("results") or [{}])[0].get("matched", []), r.get("results"))
    r = call({"op": "sessions", "query": "app.tsx"})
    check("by text in a command (a file it read)", [x["session"] for x in r.get("results", [])] == [s2], r.get("results"))
    r = call({"op": "sessions", "query": "shop front"})
    check("by the folder's name", {s1, s2, s3} <= {x["session"] for x in r.get("results", [])}, r.get("results"))
    r = call({"op": "sessions", "agent": "codex", "k": 50})
    check("--agent codex", s2 in [x["session"] for x in r.get("results", [])] and all(x["agent"] == "codex" for x in r.get("results", [])), r.get("results"))
    r = call({"op": "sessions", "query": "today", "k": 50})
    got = {x["session"] for x in r.get("results", [])}
    check("time words: today's sessions, not the one two days ago", {s2, s3} <= got and s1 not in got and r.get("window") == "today", r)
    out = cli("resume", "database", "migration", "--print")
    check("reman resume --print: the command line", out.strip() == f"claude --resume {s1}", out)
    out = cli("resume", "zzqq-nothing-like-this")
    check("reman resume with no match says so", "No agent session ran anything like" in out, out)
    out = cli("sessions", "migration")
    check("reman sessions prints it", "claude-code" in out and "alembic upgrade head" in out and f"claude --resume {s1}" in out, out)


if __name__ == "__main__":
    sys.exit(main())
