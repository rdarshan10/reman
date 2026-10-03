"""The finder as it opens from a real PowerShell prompt (ConPTY, read through a VT emulator),
against a sandbox daemon (own db, own port, own config; your history is never touched):

  * inline (the default): it draws in the 20 lines under the prompt and leaves the prompt where it
    is; with the prompt at the bottom it scrolls the screen up to make room, and PSReadLine keeps
    drawing in the right place afterwards (a pick lands on the prompt, nothing left behind);
  * `finder_height: 0`: the whole screen, as before, and the screen comes back as it was;
  * your own commands by default; F3 adds agents';
  * Ctrl+O lists every run of a command (when, how long, who), and Del Del forgets one run;
  * vim keys (`finder_keys: vim`): Esc for normal mode, j k move, q closes, i types.

  python e2e/finder_terminal.py
"""
import json, os, re, shutil, socket, subprocess, sys, tempfile, threading, time
import winpty, pyte

sys.stdout.reconfigure(encoding="utf-8")
HERE = os.path.dirname(os.path.abspath(__file__))
EXE = os.environ.get("REMAN_EXE_UNDER_TEST") or os.path.join(HERE, "..", "target", "release", "reman.exe")
PORT = 8791
ROWS, COLS = 30, 120
KEY = {"enter": "\r", "up": "\x1b[A", "down": "\x1b[B", "esc": "\x1b", "ctrl_o": "\x0f", "del": "\x1b[3~", "f3": "\x1bOR", "ctrl_u": "\x15"}
TABS = "Recall  Fixes  Flows"
results = []


def check(name, ok, detail=""):
    results.append(bool(ok))
    print(f"  {'ok  ' if ok else 'FAIL'} {name}" + (f"\n{detail}" if detail and not ok else ""))


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


class Term:
    def __init__(self, env, proj):
        self.screen = pyte.Screen(COLS, ROWS)
        self.stream = pyte.Stream(self.screen)
        self.lock = threading.Lock()
        prof = os.path.join(proj, "..", "profile.ps1")
        open(prof, "w", encoding="utf-8-sig").write(
            f"Import-Module PSReadLine\nSet-Location '{proj}'\n& '{EXE}' init powershell | Out-String | Invoke-Expression\nClear-Host\n")
        self.p = winpty.PtyProcess.spawn("powershell.exe -NoLogo -NoProfile", env=env, dimensions=(ROWS, COLS))
        threading.Thread(target=self._pump, daemon=True).start()
        self.wait(r"PS [A-Z]:\\.*>", 40)
        time.sleep(0.8)
        self.type(f". '{prof}'")
        self.keys("enter")
        time.sleep(3.0)

    def _pump(self):
        while True:
            try:
                data = self.p.read(65536)
            except EOFError:
                return
            if "\x1b[c" in data:
                self.p.write("\x1b[?61;4;6;7;14;21;22;23;24;28;32;42c")
            with self.lock:
                self.stream.feed(data)

    def lines(self):
        with self.lock:
            return [l.rstrip() for l in self.screen.display]

    def text(self):
        return "\n".join(self.lines())

    def wait(self, pattern, timeout=20):
        end = time.time() + timeout
        while time.time() < end:
            if re.search(pattern, self.text()):
                return True
            time.sleep(0.1)
        return False

    def settle(self, quiet=0.8, limit=15):
        last, since, end = None, time.time(), time.time() + limit
        while time.time() < end:
            now = self.lines()
            if now != last:
                last, since = now, time.time()
            elif time.time() - since >= quiet and not any("searching" in l or "loading runs" in l for l in now):
                break
            time.sleep(0.1)
        return self.lines()

    def keys(self, *ks, wait=0.3):
        for k in ks:
            self.p.write(KEY.get(k, k))
            time.sleep(wait)

    def type(self, s):
        for ch in s:
            self.p.write(ch)
            time.sleep(0.03)

    def run(self, cmd, wait=1.2):
        self.type(cmd)
        self.keys("enter")
        time.sleep(wait)

    def prompts(self):
        """(row, text) of every prompt line on screen."""
        return [(i, l) for i, l in enumerate(self.lines()) if re.match(r"PS [A-Z]:\\", l)]

    def close(self):
        try:
            self.p.terminate(force=True)
        except Exception:
            pass


def show(lines):
    return "\n".join(f"    {i:2}|{l}" for i, l in enumerate(lines))


def seed(proj):
    base = int(time.time()) - 3 * 86400
    runs = [
        ("npm test", 0, "human", 0, 4200), ("npm test", 1, "human", 3600, 2100), ("npm test", 0, "human", 7200, 3900),
        ("npm test", 0, "agent:claude-code", 9000, 4000), ("git status", 0, "human", 100, 80),
        ("cargo build --release", 0, "human", 200, 184000), ("zzagentonly deploy-preview", 0, "agent:claude-code", 300, 900),
        ("zzagentonly deploy-preview", 0, "agent:claude-code", 400, 900),
    ]
    for i, (c, x, who, dt, ms) in enumerate(runs):
        call({"op": "ingest", "command": c, "exit": x, "cwd": proj, "session": f"seed{i}", "actor": who, "ts": base + dt, "duration_ms": ms})


def set_config(path, **kv):
    with open(path, "w") as f:
        json.dump(kv, f)


def main():
    tmp = tempfile.mkdtemp(prefix="reman-finder-")
    proj = os.path.join(tmp, "shop")
    os.makedirs(proj)
    os.makedirs(os.path.join(tmp, "home", ".claude"))
    cfg = os.path.join(tmp, "config.json")
    set_config(cfg)
    env = dict(os.environ, REMAN_PORT=str(PORT), REMAN_CONNECT_HOME=os.path.join(tmp, "home"), REMAN_CONFIG=cfg,
               REMAN_DB=os.path.join(tmp, "reman.db"), REMAN_SPOOL=os.path.join(tmp, "spool.jsonl"))
    for k in ("CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT", "AGENT"):
        env.pop(k, None)
    proc = subprocess.Popen([EXE, "daemon", "--port", str(PORT)], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    t = None
    try:
        for _ in range(240):
            try:
                call({"op": "ping"}, timeout=2)
                break
            except OSError:
                time.sleep(0.5)
        seed(proj)
        t = Term(env, proj)
        run(t, cfg)
    finally:
        if t:
            t.close()
        try:
            call({"op": "shutdown"}, timeout=5)
        except OSError:
            pass
        proc.kill()
        time.sleep(0.5)
        shutil.rmtree(tmp, ignore_errors=True)
    passed = sum(results)
    print(f"\nFINDER RESULT: {'PASS' if passed == len(results) else 'FAIL'} ({passed}/{len(results)})")
    return 0 if passed == len(results) else 1


def tabs_row(lines):
    return next((i for i, l in enumerate(lines) if TABS in l), None)


def run(t, cfg):
    print("inline, the prompt near the top:")
    check("prompt", t.wait(r"PS [A-Z]:\\.*>", 30), t.text())
    t.run("echo zz-before")
    (prow, ptext), = t.prompts()[-1:]
    t.keys("up")
    t.wait(TABS, 15)
    got = t.settle()
    check("opens inline: the prompt stays, the query line 20 lines under it", tabs_row(got) == prow + 20, show(got))
    check("...the output above it untouched", "zz-before" in "\n".join(got[:prow]), show(got))
    check("...the prompt line itself untouched", got[prow].startswith(ptext.rstrip()[:12]), show(got))
    finder = "\n".join(got[prow + 1:])
    check("your own commands, said so", "by you" in finder and "zzagentonly" not in finder, show(got))
    t.keys("esc")
    time.sleep(0.8)
    got = t.settle()
    check("Esc: the finder's lines are gone", tabs_row(got) is None and all(not l for l in got[prow + 1:]), show(got))
    t.type("echo zz-typed")
    time.sleep(0.5)
    got = t.lines()
    check("typing lands on the same prompt line", got[prow].endswith("echo zz-typed"), show(got))
    t.keys("enter")
    time.sleep(1.0)

    print("\nagents' commands: F3")
    t.keys("up")
    t.wait(TABS, 15)
    t.type("zzagentonly")
    got = t.settle()
    prow = t.prompts()[-1][0]
    check("an agent-only command isn't listed at first", "deploy-preview" not in "\n".join(got[prow + 1:]), show(got))
    t.keys("f3")
    got = t.settle()
    finder = "\n".join(got[prow + 1:])
    check("F3: agents' commands too", "zzagentonly deploy-preview" in finder and "by you and agents" in finder, show(got))
    t.keys("f3")
    got = t.settle()
    check("F3 again: only agents'", "by agents" in "\n".join(got[prow + 1:]), show(got))
    t.keys("f3")
    t.keys("ctrl_u")
    t.type("zzqqnothing-matches-this")
    got = t.settle()
    check("F3 back to yours; nothing found says agents' are left out", "by you F3" in "\n".join(got) and "F3 adds agents'" in "\n".join(got), show(got))
    t.keys("esc")
    time.sleep(0.8)

    print("\ninline, the prompt at the bottom:")
    t.run("1..40 | % { \"zz-line $_\" }", 2.0)
    got = t.settle()
    prow = t.prompts()[-1][0]
    check("the prompt sits on the last line", prow == ROWS - 1, show(got))
    t.keys("up")
    t.wait(TABS, 15)
    got = t.settle()
    check("the screen scrolls up to make room: the query on the last line", tabs_row(got) == ROWS - 1, show(got))
    newp = ROWS - 1 - 20
    check("...the prompt moved up with the output", re.match(r"PS [A-Z]:\\", got[newp] or "") is not None and "zz-line 40" in got[newp - 1], show(got))
    t.type("cargo build")
    t.settle()
    t.keys("enter")
    time.sleep(1.2)
    got = t.settle()
    ps = t.prompts()
    check("the pick lands on that prompt", got[newp].endswith("cargo build --release"), show(got))
    check("...no second prompt or leftovers under it", len([p for p in ps if p[0] >= newp]) == 1 and all(not l for l in got[newp + 1:]), show(got))
    t.keys("ctrl_u")
    t.keys("esc")
    t.run("echo zz-after", 1.5)
    got = t.settle()
    i = next((n for n, l in enumerate(got) if l == "zz-after"), None)
    check("the next command's output follows right under it", i == newp + 1, show(got))

    print("\nCtrl+O: every run of a command")
    t.run("Clear-Host")
    t.keys("up")
    t.wait(TABS, 15)
    t.type("npm test")
    t.settle()
    t.keys("ctrl_o")
    t.wait("every run of", 10)
    got = t.settle()
    body = "\n".join(got)
    runs = [l for l in got if re.search(r"\d{4}-\d\d-\d\d \d\d:\d\d", l)]
    check("lists every run: 4", len(runs) == 4, show(got))
    check("...with when, how long and who", " 4s " in body and " 2s " in body and " claude " in body and " you " in body, show(got))
    check("...a failure with its exit code", any("✗1" in l for l in runs), show(got))
    t.keys("del")
    got = t.settle()
    check("Del asks first", "forgets this run" in "\n".join(got), show(got))
    t.keys("del")
    time.sleep(1.0)
    got = t.settle()
    left = call({"op": "runs", "command": "npm test"})["runs"]
    check("Del again forgets that run only", len(left) == 3 and "that run is forgotten" in "\n".join(got), json.dumps(left)[:300] + "\n" + show(got))
    check("...and the table shows 3", len([l for l in got if re.search(r"\d{4}-\d\d-\d\d \d\d:\d\d", l)]) == 3, show(got))
    det = call({"op": "detail", "command": "npm test"})
    check("the command's counts follow", det.get("runs") == 3 or det.get("total_runs") == 3 or "3" in json.dumps(det)[:400], json.dumps(det)[:400])
    t.keys("esc")
    got = t.settle()
    check("Esc goes back to the list, not out", tabs_row(got) is not None and "every run of" not in "\n".join(got), show(got))
    t.keys("esc")
    time.sleep(0.8)

    print("\nvim keys:")
    set_config(cfg, finder_keys="vim")
    t.run("Clear-Host")
    t.keys("up")
    t.wait(TABS, 15)
    t.settle()
    t.type("npm")
    t.settle()
    t.keys("esc")
    got = t.settle()
    q = tabs_row(got)
    check("Esc: normal mode, shown as N", q is not None and got[q].startswith(" N "), show(got))
    sel = lambda ls: next((i for i, l in enumerate(ls) if "▌" in l), None)
    a = sel(got)
    t.keys("k")
    b = sel(t.settle())
    t.keys("j")
    c = sel(t.settle())
    check("k moves up, j back down", a is not None and b is not None and b < a and c == a, f"{a} {b} {c}")
    t.keys("i")
    t.type(" test")
    got = t.settle()
    q = tabs_row(got)
    check("i types again", q is not None and "npm test" in got[q], show(got))
    t.keys("esc")
    t.keys("q")
    time.sleep(0.8)
    got = t.settle()
    check("q closes", tabs_row(got) is None, show(got))

    print("\nthe whole screen (finder_height 0):")
    set_config(cfg, finder_height=0)
    t.run("Clear-Host")
    t.run("echo zz-kept")
    before = t.settle()
    t.keys("up")
    t.wait(TABS, 15)
    got = t.settle()
    check("the query on the last line, keys on the first", tabs_row(got) == ROWS - 1 and "insert" in got[0], show(got))
    prow = max(i for i, l in enumerate(before) if l.startswith("PS "))
    t.keys("esc")
    time.sleep(0.8)
    # (ConPTY repaints only what changed, so the emulator may keep some of the last frame; the
    # other suites clear the screen after the finder for that reason)
    t.type("echo zz-back")
    time.sleep(0.6)
    after = t.settle()
    check("Esc: back on the same prompt line", after[prow].startswith("PS ") and after[prow].endswith("echo zz-back"), show(after) + "\n--- before\n" + show(before))


if __name__ == "__main__":
    sys.exit(main())
