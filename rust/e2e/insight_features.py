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
    env = dict(os.environ, REMAN_DB=os.path.join(tmp, "reman.db"), REMAN_PORT=str(PORT), REMAN_SPOOL=os.path.join(tmp, "spool.jsonl"))
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


if __name__ == "__main__":
    sys.exit(main())
