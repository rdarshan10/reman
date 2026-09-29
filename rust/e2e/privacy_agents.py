"""Privacy at capture, agent capture, and fixes keyed by the error, end to end on a sandbox daemon
(own db, own port, own config; the connector checks write into a sandboxed home):

  * secrets are masked before anything is stored, and never reach the database file; a secret
    that can't be located is not stored at all; "secrets": "drop" drops them all;
  * ignore_commands / ignore_folders in config.json keep commands out, picked up without a restart;
  * `reman scrub` masks secrets in history stored before (a dry run first);
  * Claude Code: PreToolUse + PostToolUse give a duration, a failure keeps what it printed;
  * Codex: recorded with an unknown outcome (its hooks carry no exit code), also its old
    ["bash", "-lc", "..."] form; `reman connect codex` writes its hooks.json, disconnect removes it;
  * the same error, a different command: what fixed one is offered for the other.

  python e2e/privacy_agents.py
"""
import json, os, shutil, socket, sqlite3, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
BIN = os.path.join(HERE, "..", "target", "release")
EXE = os.path.join(BIN, "reman.exe" if os.name == "nt" else "reman")
HOOK = os.path.join(BIN, "reman-hook.exe" if os.name == "nt" else "reman-hook")
PORT = 8792
SECRET = "abc123supersecretvalue"
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


class Sandbox:
    def __init__(self):
        self.tmp = tempfile.mkdtemp(prefix="reman-privacy-")
        self.cfg = os.path.join(self.tmp, "config.json")
        self.db = os.path.join(self.tmp, "reman.db")
        self.home = os.path.join(self.tmp, "home")
        os.makedirs(self.home)
        self.env = dict(os.environ, REMAN_DB=self.db, REMAN_PORT=str(PORT), REMAN_SPOOL=os.path.join(self.tmp, "spool.jsonl"),
                        REMAN_CONFIG=self.cfg, REMAN_CONNECT_HOME=self.home)
        self.config({})
        self.proc = None

    def config(self, c):
        json.dump(dict({"strict_secrets": True}, **c), open(self.cfg, "w"))
        # the daemon re-reads it when its modified time changes
        t = time.time() + 1
        os.utime(self.cfg, (t, t))

    def start(self):
        self.proc = subprocess.Popen([EXE, "daemon", "--port", str(PORT)], env=self.env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(240):
            try:
                call({"op": "ping"}, timeout=2)
                return
            except OSError:
                time.sleep(0.5)
        sys.exit("daemon did not start")

    def stop(self):
        try:
            call({"op": "shutdown"}, timeout=5)
        except OSError:
            pass
        if self.proc:
            self.proc.kill()
            self.proc.wait()

    def hook(self, agent, payload):
        return subprocess.run([HOOK, agent], input=json.dumps(payload), env=self.env, capture_output=True, text=True, timeout=20)

    def rows(self, sql, *args):
        c = sqlite3.connect(self.db)
        try:
            return c.execute(sql, args).fetchall()
        finally:
            c.close()

    def raw_bytes(self):
        data = b""
        for f in os.listdir(self.tmp):
            if f.startswith("reman.db"):
                data += open(os.path.join(self.tmp, f), "rb").read()
        return data


def ingest(cmd, cwd=r"C:\work\app", exit=0, session="s1", error=None, ts=None):
    req = {"op": "ingest", "command": cmd, "exit": exit, "cwd": cwd, "session": session, "actor": "human"}
    if error:
        req["error"] = error
    if ts:
        req["ts"] = ts
    return call(req)


def known(cmd):
    return call({"op": "detail", "command": cmd}).get("found") is not False


def main():
    sb = Sandbox()
    try:
        # --- history stored BEFORE secrets were masked: planted raw for `reman scrub` --------
        sb.start()
        ingest("git status")
        sb.stop()
        c = sqlite3.connect(sb.db)
        now = int(time.time())
        c.execute("INSERT INTO commands (cmd_text, cmd_hash, cwd, first_seen, last_used, run_count, success_count, fail_count, last_exit, human_runs, agent_runs, last_actor, desc_source) VALUES (?,?,?,?,?,3,3,0,0,3,0,'human','none')",
                  (f"export DB_PASSWORD={SECRET}", "old-secret-1", r"C:\work\app", now - 9 * 86400, now - 9 * 86400))
        cid = c.execute("SELECT last_insert_rowid()").fetchone()[0]
        for k in range(3):
            c.execute("INSERT INTO executions (command_id, actor, exit, cwd, session, ts) VALUES (?,?,0,?,?,?)", (cid, "human", r"C:\work\app", "old", now - 9 * 86400 + k))
        c.commit()
        c.close()
        sb.start()
        privacy(sb)
        agents(sb)
        same_error(sb)
    finally:
        sb.stop()
        shutil.rmtree(sb.tmp, ignore_errors=True)
    passed = sum(results)
    print(f"\nPRIVACY+AGENTS RESULT: {'PASS' if passed == len(results) else 'FAIL'} ({passed}/{len(results)})")
    return 0 if passed == len(results) else 1


def privacy(sb):
    print("privacy at capture")
    ingest(f"export API_TOKEN={SECRET}")
    check("a secret is masked when recorded", known("export API_TOKEN=***"))
    ingest("curl -H 'x-key: Ab3kL9mQ2pR7sT1vW5yZ8aC4dF6gH0jK2lN4pQ7rS9tU' https://api.example.com")
    check("a bare token is masked in place, the command kept", known("curl -H 'x-key: ***' https://api.example.com"))
    check("...and never reaches the database", not any("Ab3kL9mQ2pR7" in r[0] for r in sb.rows("SELECT cmd_text FROM commands")))
    long_path = r"ls C:\Users\Dev1\AppData\Local\Temp\claude\c--Users-dev-reman\48361776-88a3-41ab-a8a9-31cdc8b91774\scratchpad"
    ingest(long_path)
    check("a long path is not mistaken for a secret", known(long_path))
    sb.config({"ignore_commands": ["^vault "], "ignore_folders": ["(?i)private-notes"]})
    ingest("vault read secret/payments")
    check("ignore_commands keeps a command out (no restart)", not known("vault read secret/payments"))
    ingest("cat todo.txt", cwd=r"C:\Users\me\private-notes")
    check("ignore_folders keeps a folder out", not known("cat todo.txt"))
    ingest("npm run dev")
    check("everything else is still recorded", known("npm run dev"))
    sb.config({"secrets": "drop"})
    ingest("export GITHUB_TOKEN=ghx123")
    check('"secrets": "drop" records no command holding one', not known("export GITHUB_TOKEN=***") and not known("export GITHUB_TOKEN=ghx123"))
    sb.config({})

    print("reman scrub")
    out = subprocess.run([EXE, "scrub"], env=sb.env, capture_output=True, text=True, encoding="utf-8").stdout
    check("a dry run says what it would mask", "1 command(s) would be kept with the secret masked" in out and "DB_PASSWORD=***" in out, out)
    check("...and changes nothing", sb.rows("SELECT count(*) FROM commands WHERE cmd_text LIKE ?", f"%{SECRET}%")[0][0] == 1)
    out = subprocess.run([EXE, "scrub", "--apply"], env=sb.env, capture_output=True, text=True, encoding="utf-8").stdout
    check("--apply masks it", sb.rows("SELECT count(*) FROM commands WHERE cmd_text LIKE ?", f"%{SECRET}%")[0][0] == 0, out)
    d = call({"op": "detail", "command": "export DB_PASSWORD=***"})
    check("...keeping its runs", d.get("runs") == 3, d)
    check("the secret is nowhere in the database files (scrub compacts them)", SECRET.encode() not in sb.raw_bytes())


def agents(sb):
    print("agents")
    cwd = r"C:\work\app"
    sb.hook("claude", {"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_use_id": "tu-1", "session_id": "c1", "cwd": cwd, "tool_input": {"command": "npm run build"}})
    time.sleep(1.2)
    sb.hook("claude", {"hook_event_name": "PostToolUse", "tool_name": "Bash", "tool_use_id": "tu-1", "session_id": "c1", "cwd": cwd,
                       "tool_input": {"command": "npm run build"}, "tool_response": {"stdout": "built", "stderr": ""}})
    time.sleep(0.5)
    r = sb.rows("SELECT e.duration_ms, e.exit, e.actor FROM executions e JOIN commands c ON c.id = e.command_id WHERE c.cmd_text = 'npm run build'")
    check("Claude: PreToolUse + PostToolUse give a duration", r and r[0][0] is not None and r[0][0] >= 1000, r)
    check("...recorded as worked, by claude-code", r and r[0][1] == 0 and r[0][2] == "agent:claude-code", r)
    sb.hook("claude", {"hook_event_name": "PostToolUseFailure", "tool_name": "Bash", "tool_use_id": "tu-2", "session_id": "c1", "cwd": cwd,
                       "tool_input": {"command": "npm run typecheck"}, "error": "src/app.ts(3,1): error TS2304: Cannot find name 'foo'."})
    time.sleep(0.5)
    c = call({"op": "mcp", "tool": "reman_check", "args": {"command": "npm run typecheck"}, "allow_global": True})
    check("a failure keeps what it printed (reman_check last_error)", "Cannot find name" in (c.get("last_error") or ""), c)
    sb.hook("codex", {"hook_event_name": "PostToolUse", "tool_name": "Bash", "tool_use_id": "cx-1", "session_id": "x1", "cwd": cwd,
                      "tool_input": {"command": "cargo test"}, "tool_response": "test result: ok. 12 passed"})
    sb.hook("codex", {"hook_event_name": "PostToolUse", "tool_name": "shell", "tool_use_id": "cx-2", "session_id": "x1", "cwd": cwd,
                      "tool_input": {"command": ["bash", "-lc", "cargo fmt --check"]}, "tool_response": ""})
    time.sleep(0.5)
    r = sb.rows("SELECT c.cmd_text, e.exit, e.actor FROM executions e JOIN commands c ON c.id = e.command_id WHERE e.actor = 'agent:codex' ORDER BY e.id")
    check("Codex: recorded, tagged codex, outcome unknown (no exit code in its hooks)", [x[0] for x in r] == ["cargo test", "cargo fmt --check"] and all(x[1] is None for x in r), r)

    print("connectors")
    env = sb.env
    os.makedirs(os.path.join(sb.home, ".codex"), exist_ok=True)
    out = subprocess.run([EXE, "connect", "codex"], env=env, capture_output=True, text=True, encoding="utf-8")
    hooks = os.path.join(sb.home, ".codex", "hooks.json")
    hj = json.load(open(hooks)) if os.path.exists(hooks) else {}
    events = set(hj.get("hooks", {}))
    cmd = (hj.get("hooks", {}).get("PostToolUse") or [{}])[0].get("hooks", [{}])[0].get("command", "")
    check("connect codex writes hooks.json (before + after + failure)", {"PreToolUse", "PostToolUse", "PostToolUseFailure"} <= events, out.stdout + out.stderr)
    check("...running reman-hook codex", "reman-hook" in cmd and cmd.endswith(" codex"), cmd)
    check("...and says to approve them in Codex", "/hooks" in out.stdout + out.stderr, out.stdout)
    out = subprocess.run([EXE, "connect", "claude-code"], env=env, capture_output=True, text=True, encoding="utf-8")
    cs = os.path.join(sb.home, ".claude", "settings.json")
    cj = json.load(open(cs)) if os.path.exists(cs) else {}
    check("connect claude-code adds the PreToolUse hook (durations)", "PreToolUse" in cj.get("hooks", {}), out.stdout + out.stderr)
    subprocess.run([EXE, "disconnect", "codex"], env=env, capture_output=True, text=True)
    left = open(hooks).read() if os.path.exists(hooks) else ""
    check("disconnect codex removes its hooks", "reman" not in left, left)


def same_error(sb):
    print("the same error, a different command")
    err = "npm ERR! code ERESOLVE\nnpm ERR! ERESOLVE unable to resolve dependency tree"
    t = int(time.time()) - 3600
    ingest("npm install", exit=1, session="e1", error=err, ts=t)
    ingest("npm install --legacy-peer-deps", session="e1", ts=t + 30)
    r = ingest("npm ci", exit=1, session="e2", error=err)
    sg = r.get("suggest") or {}
    check("a different command with the same error gets what fixed it", sg.get("kind") == "same_error" and sg.get("command") == "npm install --legacy-peer-deps" and sg.get("failed") == "npm install", r)
    f = call({"op": "mcp", "tool": "reman_fixes", "args": {"failed_command": "npm ci", "error": err}, "allow_global": True})
    first = f[0] if isinstance(f, list) and f else {}
    check("reman_fixes with the error: same_error first", first.get("same_error") and first.get("fixed_command") == "npm install --legacy-peer-deps" and first.get("was_fixing") == "npm install", f)
    r = ingest("npm run lint", exit=1, session="e3", error="eslint: 3 problems")
    check("no match when the error is different", (r.get("suggest") or {}).get("kind") != "same_error", r)


if __name__ == "__main__":
    sys.exit(main())
