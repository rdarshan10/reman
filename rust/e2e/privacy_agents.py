"""Privacy at capture, agent capture, and fixes keyed by the error, end to end on a sandbox daemon
(own db, own port, own config; the connector checks write into a sandboxed home):

  * secrets are masked before anything is stored, and never reach the database file; a secret
    that can't be located is not stored at all; "secrets": "drop" drops them all;
  * ignore_commands / ignore_folders in config.json keep commands out, picked up without a restart;
  * `reman scrub` masks secrets in history stored before (a dry run first);
  * Claude Code: PreToolUse + PostToolUse give a duration, a failure keeps what it printed;
  * Codex: recorded with an unknown outcome (its hooks carry no exit code), also its old
    ["bash", "-lc", "..."] form; `reman connect codex` writes its hooks.json, disconnect removes it;
  * the same error, a different command: what fixed one is offered for the other;
  * redaction off ("secrets": "keep"): commands and their errors are stored as typed and the
    user's finder shows them whole, while agents still get them masked;
  * what an agent is told: nothing is visible until the user approves a folder, not even its own
    project, and it is told why and how to ask; an agent can't approve a folder for itself; once
    approved, the project's commands come first, a query naming a tool never gets another tool's
    command, commands that only failed are counted when left out, and an empty answer says so.

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
        # (REMAN_CONSENT_WINDOW=off: a test never pops reman's own question window on your screen)
        self.env = dict(os.environ, REMAN_DB=self.db, REMAN_PORT=str(PORT), REMAN_SPOOL=os.path.join(self.tmp, "spool.jsonl"),
                        REMAN_CONFIG=self.cfg, REMAN_CONNECT_HOME=self.home, REMAN_CONSENT_WINDOW="off")
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
        more_agents(sb)
        same_error(sb)
        agent_answers(sb)
        consent(sb)
        kept_as_typed(sb)
    finally:
        sb.stop()
        shutil.rmtree(sb.tmp, ignore_errors=True)
    passed = sum(results)
    print(f"\nPRIVACY+AGENTS RESULT: {'PASS' if passed == len(results) else 'FAIL'} ({passed}/{len(results)})")
    return 0 if passed == len(results) else 1


AGENT_VARS = ("CLAUDECODE", "GEMINI_CLI", "CLAUDE_CODE_ENTRYPOINT")


def as_user(env):
    """The environment of the user's own terminal: none of the variables a coding agent sets."""
    return {k: v for k, v in env.items() if k not in AGENT_VARS and not k.startswith("CODEX_") and k != "REMAN_MCP_ROOT"}


def agent_call(sb, cwd, tool, args):
    """`reman call` from `cwd`: the agent bridge (policy from the env + the folder it runs in)."""
    p = subprocess.run([EXE, "call", tool, json.dumps(args)], cwd=cwd, env=as_user(sb.env), capture_output=True, text=True, encoding="utf-8", timeout=60)
    body, _, note = p.stdout.partition("\nnote: ")
    try:
        return json.loads(body), note.strip()
    except ValueError:
        return None, p.stdout + p.stderr


def connect(sb, *args, agent=False):
    env = dict(as_user(sb.env), **({"CLAUDECODE": "1"} if agent else {}))
    p = subprocess.run([EXE, "connect", *args], env=env, capture_output=True, text=True, encoding="utf-8", timeout=60)
    return p.returncode, p.stdout + p.stderr


def agent_answers(sb):
    print("\nwhat an agent is told (nothing is visible until the user approves a folder)")
    site = os.path.join(sb.tmp, "site")          # the agent's project: a repo, not approved at first
    shared = os.path.join(sb.tmp, "shared")      # a folder the user approved
    for d in (os.path.join(site, ".git"), os.path.join(site, "src"), shared):
        os.makedirs(d, exist_ok=True)
    sb.config({"mcp_roots": [shared]})
    for _ in range(3):
        ingest("npx vercel --prod", cwd=site)
        ingest("npm run dev", cwd=site)
        ingest("npx astro check", cwd=site)
        ingest("npx expo start --dev-client", cwd=shared)
        ingest("npm start", cwd=shared)
        ingest("npx netlify deploy --prod", cwd=site, exit=1)
    call({"op": "ingest", "command": "npx vercel logs", "exit": None, "cwd": site, "session": "s1", "actor": "human"})

    got, note = agent_call(sb, os.path.join(site, "src"), "reman_search", {"intent": "deploy to vercel"})
    check("its own project is hidden until approved", got == [], got)
    check("  and it is told why, and to ask the user", "is not shared with agents" in note and "--add-root" in note and "Do not run that yourself" in note, note)
    got, note = agent_call(sb, site, "reman_recent", {})
    check("  recent: nothing from it either", not any(x.get("cwd", "").startswith(site) for x in got or []), got)

    got, note = agent_call(sb, site, "reman_recent", {"cwd": site})
    check("asking about it by folder: empty, and told why", got == [] and "not a folder this user shares" in note, (got, note))

    code, out = connect(sb, "--add-root", site, agent=True)
    check("an agent cannot approve it for itself (reman connect under Claude Code refuses)", code != 0 and "approval" in out and site not in json.dumps(json.load(open(sb.cfg))), out)
    code, out = connect(sb, "--old-history", "on", agent=True)
    check("  nor share old history", code != 0, out)
    code, out = connect(sb, "--add-root", site)
    check("the user approves it in their own terminal", code == 0 and "agents may now see" in out, out)
    time.sleep(1.2)

    got, note = agent_call(sb, os.path.join(site, "src"), "reman_search", {"intent": "deploy to vercel"})
    cmds = [x["command"] for x in got or []]
    check("approved: its history shows (started in a subfolder)", "npx vercel --prod" in cmds and not note, (got, note))
    check("  marked in_this_project", any(x.get("in_this_project") for x in got or []), got)
    check("  a command with no recorded outcome stays (success_rate null)", any(x["command"] == "npx vercel logs" and x.get("success_rate") is None for x in got or []), got)

    got, note = agent_call(sb, site, "reman_search", {"intent": "start the astro dev server"})
    wrong = [c for c in (x["command"] for x in got or []) if "astro" not in c]
    check("a query naming astro never gets another tool's command", not wrong, wrong)

    got, note = agent_call(sb, site, "reman_search", {"intent": "deploy to netlify"})
    check("a command that only failed is left out, and counted", got == [] and "only ever failed" in note, (got, note))
    got, _ = agent_call(sb, site, "reman_search", {"intent": "deploy to netlify", "worked_only": False})
    check("  worked_only=false shows it", any("netlify" in x["command"] for x in got or []), got)

    got, note = agent_call(sb, site, "reman_search", {"intent": "flash firmware to the arduino"})
    check("nothing close: empty, and it says so", got == [] and "Nothing close" in note, (got, note))

    code, out = connect(sb, "--remove-root", site, agent=True)
    check("taking access away needs no approval", code == 0 and "no longer see" in out, out)


class McpSession:
    """`reman mcp` over its real stdio wire, as an app: it answers reman's own questions too."""

    def __init__(self, sb, cwd, can_ask=True, env=None):
        self.p = subprocess.Popen([EXE, "mcp"], cwd=cwd, env=dict(as_user(sb.env), **(env or {})), stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=subprocess.DEVNULL, text=True, encoding="utf-8")
        self.n = 0
        caps = {"elicitation": {}} if can_ask else {}
        self.rpc("initialize", {"protocolVersion": "2025-06-18", "capabilities": caps, "clientInfo": {"name": "TestApp", "version": "1"}})
        self.asked = []

    def send(self, msg):
        self.p.stdin.write(json.dumps(msg) + "\n")
        self.p.stdin.flush()

    def rpc(self, method, params, answer=None):
        """A request; when reman asks the user something meanwhile, `answer` is the user's choice."""
        self.n += 1
        self.send({"jsonrpc": "2.0", "id": self.n, "method": method, "params": params})
        while True:
            msg = json.loads(self.p.stdout.readline())
            if msg.get("method") == "elicitation/create":
                self.asked.append(msg["params"]["message"])
                result = {"action": "accept", "content": {"share": answer}} if answer else {"action": "decline"}
                self.send({"jsonrpc": "2.0", "id": msg["id"], "result": result})
                continue
            if msg.get("id") == self.n:
                return msg.get("result", {})

    def tool(self, name, args, answer=None):
        r = self.rpc("tools/call", {"name": name, "arguments": args}, answer)
        return r.get("structuredContent", {})

    def close(self):
        self.p.stdin.close()
        self.p.wait(timeout=10)


def consent(sb):
    print("\nthe agent asks, the user says yes (MCP elicitation): shared for this session, or always")
    repo = os.path.join(sb.tmp, "consented")
    os.makedirs(os.path.join(repo, ".git"), exist_ok=True)
    for _ in range(2):
        ingest("npm run e2e-consent", cwd=repo)
    sb.config({})

    s = McpSession(sb, repo)
    got = s.tool("reman_recent", {}, answer="Allow for this session")
    cmds = [x.get("command") for x in got.get("result") or []]
    check("the app is asked, by name, about the project folder", len(s.asked) == 1 and "TestApp wants to use your command history" in s.asked[0] and "consented" in s.asked[0], s.asked)
    check("yes, for this session: the history shows at once", "npm run e2e-consent" in cmds, got)
    check("...and nothing is saved (only this session)", not json.load(open(sb.cfg)).get("mcp_roots"), json.load(open(sb.cfg)))
    got = s.tool("reman_recent", {})
    check("asked once: the next call just works", len(s.asked) == 1 and got.get("result"), got)
    s.close()

    s = McpSession(sb, repo)
    got = s.tool("reman_recent", {})  # no answer: the user declines
    check("a new session asks again; declined, nothing shows", len(s.asked) == 1 and not got.get("result"), got)
    s.tool("reman_recent", {})
    check("...and it isn't asked twice", len(s.asked) == 1, s.asked)
    s.close()

    s = McpSession(sb, repo)
    s.tool("reman_recent", {}, answer="Always allow")
    roots = json.load(open(sb.cfg)).get("mcp_roots", [])
    check("yes, always: saved like `reman connect --add-root`", any(os.path.normcase(r) == os.path.normcase(repo) for r in roots), roots)
    s.close()
    sb.config({})

    s = McpSession(sb, repo, can_ask=False)
    got = s.tool("reman_recent", {})
    check("an app that can't ask (and no window here): never asked, told how to share instead", not s.asked and not got.get("result") and "--add-root" in (got.get("note") or ""), got)
    s.close()

    # Claude Code's VS Code extension says it can ask, then declines every question unseen
    s = McpSession(sb, repo, env={"CLAUDE_CODE_ENTRYPOINT": "claude-vscode"})
    got = s.tool("reman_recent", {}, answer="Allow for this session")
    check("Claude Code in VS Code: not asked through the app (it would decline unseen)", not s.asked and not got.get("result"), (s.asked, got))
    check("...the note tells the agent to call reman_share_project", "reman_share_project" in (got.get("note") or ""), got.get("note"))
    # the agent calls it: the app shows its Allow / Deny buttons, and the call arriving here is the Allow
    shared = s.tool("reman_share_project", {}).get("result", {})
    check("the app's Allow on reman_share_project shares the project, for this session", shared.get("shared") is True and shared.get("for") == "this session", shared)
    got = s.tool("reman_recent", {})
    check("...the history shows at once", "npm run e2e-consent" in [x.get("command") for x in got.get("result") or []], got)
    check("...and nothing is saved", not json.load(open(sb.cfg)).get("mcp_roots"), json.load(open(sb.cfg)))
    s.close()

    # strict permissions: only reman's own dialog box decides (here there is none: the sandbox
    # never shows it), so neither the app's dialog nor its Allow on reman_share_project shares
    sb.config({"strict_permissions": True})
    s = McpSession(sb, repo)
    got = s.tool("reman_recent", {}, answer="Allow for this session")
    shared = s.tool("reman_share_project", {}).get("result", {})
    check("strict permissions: the app isn't asked, its Allow doesn't share", not s.asked and shared.get("shared") is False and "Strict permissions" in shared.get("reason", ""), (s.asked, shared))
    check("...and the history stays hidden", not s.tool("reman_recent", {}).get("result"))
    s.close()
    sb.config({})

    # a No in the app's own dialog stays a No: the share tool can't get around it
    s = McpSession(sb, repo)
    s.tool("reman_recent", {})  # asked through the app, the user declines
    shared = s.tool("reman_share_project", {}).get("result", {})
    got = s.tool("reman_recent", {})
    check("after the user said No, reman_share_project is refused", shared.get("shared") is False and not got.get("result"), (shared, got))
    s.close()


def kept_as_typed(sb):
    print("\nredaction off for the user (secrets kept as typed)")
    keep = os.path.join(sb.tmp, "keep")
    os.makedirs(keep, exist_ok=True)
    secret = "Kp9xQ2mZ7vT4wR8nL3"
    cmd = f"export API_TOKEN={secret}"
    sb.config({"secrets": "keep", "mcp_roots": [keep]})
    ingest(cmd, cwd=keep)
    ingest("npm publish", cwd=keep, exit=1, error=f"401 Unauthorized: API_TOKEN={secret} rejected")
    check("the command is stored as typed", sb.rows("SELECT 1 FROM commands WHERE cmd_text = ?", cmd))
    errs = [e for (e,) in sb.rows("SELECT err FROM executions WHERE err IS NOT NULL") if "401" in e]
    check("  and what a failure printed", errs and secret in errs[-1], errs)
    found = [x["command"] for x in call({"op": "search", "query": "API_TOKEN", "cwd": keep, "scope": "folder", "k": 5})["results"]]
    check("the user's finder shows it whole", cmd in found, found)
    got, note = agent_call(sb, keep, "reman_recent", {"cwd": keep})
    shown = json.dumps(got)
    check("an agent still gets it masked", got is not None and secret not in shown, shown)
    fix = call({"op": "mcp", "tool": "reman_check", "args": {"command": "npm publish", "cwd": keep}, "roots": [keep]})
    check("  including the error text agents are given", secret not in json.dumps(fix), fix)
    sb.config({})


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


def more_agents(sb):
    print("\nmore agents: each one's own hook format, through reman-hook")
    cwd = r"C:\work\tools"

    def runs(cmd):
        # only this section's folder: earlier sections ran some of the same commands elsewhere
        return sb.rows("SELECT e.actor, e.exit, e.duration_ms, e.cwd, e.seen FROM executions e JOIN commands c ON c.id = e.command_id "
                       "WHERE c.cmd_text = ? AND e.cwd LIKE '%tools%' ORDER BY e.id", cmd)

    sb.hook("cursor", {"hook_event_name": "afterShellExecution", "conversation_id": "cu1", "workspace_roots": [cwd],
                       "command": "npx jest 2>&1 | tail -3", "output": "Tests:  1 failed, 9 passed, 10 total", "duration": 2300, "sandbox": False})
    sb.hook("gemini", {"hook_event_name": "AfterTool", "session_id": "g1", "cwd": cwd, "tool_name": "run_shell_command",
                       "tool_input": {"command": "cargo test", "dir_path": "api"},
                       "tool_response": {"llmContent": "Command: cargo test\nDirectory: api\nOutput: test result: FAILED. 1 passed; 1 failed\nExit Code: 101", "returnDisplay": ""}})
    sb.hook("gemini", {"hook_event_name": "AfterTool", "session_id": "g1", "cwd": cwd, "tool_name": "read_file", "tool_input": {"file_path": "a.txt"}, "tool_response": {"llmContent": "x"}})
    sb.hook("windsurf", {"agent_action_name": "pre_run_command", "trajectory_id": "ws1", "execution_id": "wx1", "tool_info": {"command_line": "npm run dev", "cwd": cwd}})
    time.sleep(1.1)
    sb.hook("windsurf", {"agent_action_name": "post_run_command", "trajectory_id": "ws1", "execution_id": "wx1", "tool_info": {"command_line": "npm run dev", "cwd": cwd}})
    sb.hook("copilot", {"hook_event_name": "PreToolUse", "session_id": "cp", "cwd": cwd, "tool_name": "run_in_terminal", "tool_use_id": "cp1", "tool_input": {"command": "npm test"}})
    sb.hook("copilot", {"hook_event_name": "PostToolUse", "session_id": "cp", "cwd": cwd, "tool_name": "run_in_terminal", "tool_use_id": "cp1",
                        "tool_input": {"command": "npm test"}, "tool_response": "Tests:  12 passed, 12 total"})
    sb.hook("copilot", {"hook_event_name": "PostToolUse", "session_id": "cp", "cwd": cwd, "tool_name": "read_file", "tool_use_id": "cp2", "tool_input": {"filePath": "x"}})
    sb.hook("event", {"agent": "opencode", "command": "pytest -q", "cwd": cwd, "session": "o1", "exit": 1, "output": "1 failed, 2 passed in 0.31s", "duration_ms": 800})
    sb.hook("event", {"agent": "pi", "command": "ruff check .", "cwd": cwd, "exit": 0, "output": "All checks passed!", "duration_ms": 120})
    time.sleep(0.8)

    r = runs("npx jest 2>&1 | tail -3")
    check("Cursor: recorded as cursor, with its duration", r and r[0][0] == "agent:cursor" and r[0][2] == 2300, r)
    check("...no exit code from Cursor, but its output says jest failed", r and r[0][1] is None and '"npx jest",0,0,1' in (r[0][4] or ""), r)
    r = runs("cargo test")
    check("Gemini CLI: its result text gives the exit code (101), in the folder it ran in", r and r[0][0] == "agent:gemini-cli" and r[0][1] == 101 and r[0][3].replace("/", "\\").endswith(r"tools\api"), r)
    check("...and its other tools are not commands", not sb.rows("SELECT 1 FROM commands WHERE cmd_text LIKE '%a.txt%'"))
    r = runs("npm run dev")
    check("Windsurf: recorded as windsurf, a duration from pre_run_command, outcome unknown", r and r[0][0] == "agent:windsurf" and r[0][1] is None and (r[0][2] or 0) >= 1000, r)
    r = runs("npm test")
    check("VS Code (Copilot): its terminal tool, recorded as copilot, with a duration", r and r[0][0] == "agent:copilot" and r[0][2] is not None, r)
    check("...its output says the tests passed", r and '"npm test",0,1,0' in (r[0][4] or ""), r)
    r = runs("pytest -q")
    check("opencode (reman's plugin): recorded as opencode, exit 1, duration", r and r[0][0] == "agent:opencode" and r[0][1] == 1 and r[0][2] == 800, r)
    check("pi (reman's extension): recorded as pi", (runs("ruff check .") or [[None]])[0][0] == "agent:pi")

    print("\nan agent typing into your terminal is one run, not two")
    ingest("npm run lint", cwd=cwd, exit=2)  # the shell's prompt hook: a person's run, exact exit code
    sb.hook("copilot", {"hook_event_name": "PostToolUse", "session_id": "cp", "cwd": cwd, "tool_name": "run_in_terminal", "tool_use_id": "cp3",
                        "tool_input": {"command": "npm run lint"}, "tool_response": "3 problems"})
    time.sleep(0.6)
    r = runs("npm run lint")
    check("shell first: one run, the agent's, with the shell's exit code", [(x[0], x[1]) for x in r] == [("agent:copilot", 2)], r)
    h = sb.rows("SELECT human_runs, agent_runs FROM commands WHERE cmd_text = 'npm run lint'")
    check("...counted as the agent's, not a person's", h == [(0, 1)], h)
    sb.hook("cursor", {"hook_event_name": "afterShellExecution", "conversation_id": "cu1", "workspace_roots": [cwd], "command": "npm run format", "output": "", "duration": 900})
    time.sleep(0.5)
    ingest("npm run format", cwd=cwd, exit=0)
    time.sleep(0.3)
    r = runs("npm run format")
    check("agent first: one run, the agent's, learning the shell's exit code", [(x[0], x[1]) for x in r] == [("agent:cursor", 0)], r)
    # VS Code rewrites a line before typing it: `&&` becomes `;` in PowerShell, a `cd <here> &&` goes
    ingest("npm ci; npm run e2e", cwd=cwd, exit=1)
    sb.hook("copilot", {"hook_event_name": "PostToolUse", "session_id": "cp", "cwd": cwd, "tool_name": "run_in_terminal", "tool_use_id": "cp4",
                        "tool_input": {"command": "npm ci && npm run e2e", "explanation": "x", "goal": "x", "mode": "sync"}, "tool_response": "Command exited with code 1"})
    ingest("npm run build", cwd=cwd, exit=0)
    sb.hook("copilot", {"hook_event_name": "PostToolUse", "session_id": "cp", "cwd": cwd, "tool_name": "run_in_terminal", "tool_use_id": "cp5",
                        "tool_input": {"command": f"cd {cwd} && npm run build", "mode": "sync"}, "tool_response": "built"})
    time.sleep(0.6)
    both = sb.rows("SELECT c.cmd_text, e.actor FROM executions e JOIN commands c ON c.id = e.command_id WHERE e.cwd LIKE '%tools%' AND (c.cmd_text LIKE '%npm run e2e%' OR c.cmd_text LIKE '%npm run build%')")
    check("a line VS Code rewrote (&& to ;, a cd dropped) is still one run, the agent's", sorted(both) == [("npm ci; npm run e2e", "agent:copilot"), ("npm run build", "agent:copilot")], both)
    # what happened for real: VS Code opened a terminal, npm exited -4058 (no package.json)
    ingest(r'try { . "c:\Users\me\AppData\Local\Programs\Microsoft VS Code\x\resources\app\out\vs\workbench\contrib\terminal\common\scripts\shellIntegration.ps1" } catch {}', cwd=cwd, exit=1)
    check("VS Code's terminal startup line is not recorded", not sb.rows("SELECT 1 FROM commands WHERE cmd_text LIKE '%shellIntegration%'"))
    ingest("npm test", cwd=r"C:\work\winexit", exit=-4058)
    r = sb.rows("SELECT e.exit FROM executions e WHERE e.cwd = ?", r"C:\work\winexit")
    check("a negative Windows exit code (npm's -4058) is a failure, not unknown", r and r[0][0] and r[0][0] > 0, r)
    # the shell couldn't tell (no exit code), the agent could: the one run learns it
    call({"op": "ingest", "command": "npm run smoke", "exit": None, "cwd": cwd, "session": "s1", "actor": "human"})
    sb.hook("copilot", {"hook_event_name": "PostToolUse", "session_id": "cp", "cwd": cwd, "tool_name": "run_in_terminal", "tool_use_id": "cp6",
                        "tool_input": {"command": "npm run smoke"}, "tool_response": "npm ERR! Missing script: smoke\n\nCommand exited with code 1"})
    time.sleep(0.6)
    r = runs("npm run smoke")
    check("shell without an exit code, agent with one: one run, the agent's, exit 1", [(x[0], x[1]) for x in r] == [("agent:copilot", 1)], r)
    # the Copilot CLI reads the same hook file: its shell tool and its result shape
    sb.hook("copilot", {"hook_event_name": "PostToolUse", "session_id": "cli", "cwd": cwd, "tool_name": "bash", "tool_input": {"command": "make check"},
                        "tool_result": {"resultType": "failure", "textResultForLlm": "FAIL\n<exited with exit code 2>"}})
    time.sleep(0.5)
    r = runs("make check")
    check("Copilot CLI: its bash tool, the exit code from its result text", r and (r[0][0], r[0][1]) == ("agent:copilot", 2), r)
    ingest("npm run lint", cwd=r"C:\work\elsewhere", exit=0)
    check("the same command in another folder is its own run", len(sb.rows("SELECT 1 FROM executions e JOIN commands c ON c.id = e.command_id WHERE c.cmd_text = 'npm run lint'")) == 2)
    ingest("npm run lint", cwd=cwd, exit=0)
    check("...and so is a person running it again", runs("npm run lint")[-1][0] == "human", runs("npm run lint"))

    print("\nconnectors: each agent's capture, installed and removed beside the user's own")
    env, home = sb.env, sb.home

    def run(*a):
        p = subprocess.run([EXE, *a], env=env, capture_output=True, text=True, encoding="utf-8", timeout=60)
        return p.stdout + p.stderr

    def load(*p):
        f = os.path.join(home, *p)
        return json.load(open(f, encoding="utf-8")) if os.path.exists(f) else None

    os.makedirs(os.path.join(home, ".cursor"), exist_ok=True)
    json.dump({"version": 1, "hooks": {"afterShellExecution": [{"command": "./audit.sh"}], "beforeReadFile": [{"command": "./guard.sh"}]}}, open(os.path.join(home, ".cursor", "hooks.json"), "w"))
    out = run("connect", "cursor")
    ch = load(".cursor", "hooks.json") or {}
    after = [h.get("command", "") for h in ch.get("hooks", {}).get("afterShellExecution", [])]
    check("Cursor: MCP + afterShellExecution running reman-hook cursor", (load(".cursor", "mcp.json") or {}).get("mcpServers", {}).get("reman") and any("reman-hook" in c and c.endswith(" cursor") for c in after), out)
    check("...beside the user's own hooks", "./audit.sh" in after and ch["hooks"].get("beforeReadFile"), ch)
    run("disconnect", "cursor")
    ch = load(".cursor", "hooks.json") or {}
    check("...and disconnect takes only reman's out", "reman" not in json.dumps(ch) and "./audit.sh" in json.dumps(ch), ch)

    os.makedirs(os.path.join(home, ".gemini"), exist_ok=True)
    json.dump({"theme": "Dracula"}, open(os.path.join(home, ".gemini", "settings.json"), "w"))
    out = run("connect", "gemini")
    gs = load(".gemini", "settings.json") or {}
    at = (gs.get("hooks", {}).get("AfterTool") or [{}])[0]
    check("Gemini CLI: MCP + AfterTool on run_shell_command", gs.get("mcpServers", {}).get("reman") and at.get("matcher") == "run_shell_command" and at["hooks"][0]["command"].endswith(" gemini"), out)
    run("disconnect", "gemini")
    gs = load(".gemini", "settings.json") or {}
    check("...disconnect leaves the user's settings as they were", "reman" not in json.dumps(gs) and gs.get("theme") == "Dracula", gs)

    os.makedirs(os.path.join(home, ".codeium", "windsurf"), exist_ok=True)
    out = run("connect", "windsurf")
    wh = load(".codeium", "windsurf", "hooks.json") or {}
    check("Windsurf: pre_run_command + post_run_command (with a PowerShell form)", {"pre_run_command", "post_run_command"} <= set(wh.get("hooks", {})) and "powershell" in wh["hooks"]["post_run_command"][0], out)
    run("disconnect", "windsurf")
    check("...removed (the file was only reman's)", load(".codeium", "windsurf", "hooks.json") is None)

    out = run("connect", "vscode")
    vh = load(".copilot", "hooks", "reman.json") or {}
    check("VS Code: a hook file of its own in ~/.copilot/hooks (PreToolUse + PostToolUse)", {"PreToolUse", "PostToolUse"} <= set(vh.get("hooks", {})) and vh["hooks"]["PostToolUse"][0]["command"].endswith(" copilot"), out)
    e = (vh.get("hooks", {}).get("PostToolUse") or [{}])[0]
    check("...valid for the Copilot CLI too (version, bash, powershell, timeoutSec)", vh.get("version") == 1 and e.get("bash") and e.get("powershell") and e.get("timeoutSec"), vh)
    run("disconnect", "vscode")
    check("...removed", load(".copilot", "hooks", "reman.json") is None)

    os.makedirs(os.path.join(home, ".config", "opencode"), exist_ok=True)
    out = run("connect", "opencode")
    oc = load(".config", "opencode", "opencode.json") or {}
    plugin = os.path.join(home, ".config", "opencode", "plugins", "reman.ts")
    src = open(plugin, encoding="utf-8").read() if os.path.exists(plugin) else ""
    check("opencode: MCP (type local) + reman's plugin", oc.get("mcp", {}).get("reman", {}).get("type") == "local" and "tool.execute.after" in src, out)
    check("...the plugin calls this reman-hook by its full path", "reman-hook" in src and "@@REMAN_HOOK@@" not in src, src[:300])
    run("disconnect", "opencode")
    check("...both removed", not os.path.exists(plugin) and "reman" not in json.dumps(load(".config", "opencode", "opencode.json") or {}))

    os.makedirs(os.path.join(home, ".pi", "agent"), exist_ok=True)
    out = run("connect", "pi")
    ext = os.path.join(home, ".pi", "agent", "extensions", "reman.ts")
    src = open(ext, encoding="utf-8").read() if os.path.exists(ext) else ""
    check("pi: reman's extension (pi has no MCP)", "tool_execution_end" in src and "reman-hook" in src, out)
    listing = run("connect")
    check("`reman connect` lists opencode and pi", "opencode" in listing and "pi" in listing, listing)
    run("disconnect", "pi")
    check("...removed", not os.path.exists(ext))


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
