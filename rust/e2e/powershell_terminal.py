"""Drive a REAL interactive PowerShell (ConPTY) that loads the real profile, press the real keys,
and read the rendered screen through a VT emulator. Isolated: own port + a COPY of the db."""
import os, sys, time, shutil, threading, re, subprocess, json, socket
import winpty, pyte

sys.stdout.reconfigure(encoding="utf-8")
ROWS, COLS = 30, 120
TMP = os.environ["TEMP"]
DB = os.path.join(TMP, "reman-tui.db")
EXE = os.path.join(os.path.expanduser("~"), ".reman", "bin", "reman.exe")
subprocess.run([EXE, "stop"], env=dict(os.environ, REMAN_PORT="8768"), capture_output=True, timeout=30)
time.sleep(0.5)
for ext in ("", "-wal", "-shm"):
    try:
        os.remove(DB + ext)
    except FileNotFoundError:
        pass
shutil.copy(os.path.join(os.path.expanduser("~"), ".reman", "reman.db"), DB)
ENV = dict(os.environ, REMAN_PORT="8768", REMAN_DB=DB, REMAN_SPOOL=os.path.join(TMP, "reman-tui-spool.jsonl"))
for k in ("CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT", "AGENT"):  # behave like a human terminal
    ENV.pop(k, None)

KEY = {"enter": "\r", "up": "\x1b[A", "down": "\x1b[B", "left": "\x1b[D", "right": "\x1b[C", "esc": "\x1b",
       "tab": "\t", "f2": "\x1bOQ", "del": "\x1b[3~", "bs": "\x7f", "ctrl_r": "\x12", "ctrl_t": "\x14",
       "ctrl_g": "\x07", "ctrl_p": "\x10", "ctrl_u": "\x15", "alt_f": "\x1bf"}


class Term:
    def __init__(self):
        self.screen = pyte.Screen(COLS, ROWS)
        self.stream = pyte.Stream(self.screen)
        self.lock = threading.Lock()
        self.p = winpty.PtyProcess.spawn("powershell.exe -NoLogo", env=ENV, dimensions=(ROWS, COLS))
        threading.Thread(target=self._pump, daemon=True).start()

    def _pump(self):
        while True:
            try:
                data = self.p.read(65536)
            except EOFError:
                return
            with self.lock:
                self.stream.feed(data)

    def text(self):
        with self.lock:
            return "\n".join(l.rstrip() for l in self.screen.display)

    def wait(self, pattern, timeout=20):
        t = time.time()
        while time.time() - t < timeout:
            if re.search(pattern, self.text()):
                return True
            time.sleep(0.1)
        return False

    def keys(self, *ks, gap=0.15):
        for k in ks:
            self.p.write(KEY.get(k, k))
            time.sleep(gap)

    def type(self, s, gap=0.05):
        for ch in s:
            self.p.write(ch)
            time.sleep(gap)

    def prompt_line(self):
        lines = [l for l in self.text().splitlines() if l.startswith("PS ")]
        return lines[-1] if lines else ""

    def bar(self):
        b = [l for l in self.text().splitlines() if "reman>" in l]
        return b[-1] if b else ""

    def rows(self):
        return [l for l in self.text().splitlines() if l.startswith(("> ", "  "))]

    def snap(self, title):
        print(f"\n----- {title} " + "-" * max(0, COLS - 8 - len(title)))
        body = self.text().splitlines()
        while body and not body[-1].strip():
            body.pop()
        print("\n".join(body[-ROWS:]))


def finder_running():
    out = subprocess.run(["powershell", "-NoProfile", "-Command",
                          "@(Get-CimInstance Win32_Process -Filter \"Name='reman.exe'\" | Where-Object { $_.CommandLine -match ' find ' }).Count"],
                         capture_output=True, text=True).stdout.strip()
    return out not in ("", "0")


def daemon(req):
    s = socket.create_connection(("127.0.0.1", 8768), timeout=10)
    s.sendall((json.dumps(req) + "\n").encode())
    d = b""
    while not d.endswith(b"\n"):
        d += s.recv(1 << 20)
    return json.loads(d)


results = []


def check(name, ok):
    results.append((name, ok))
    print(f"  {'ok  ' if ok else 'FAIL'} {name}")


subprocess.run([EXE, "ping"], env=ENV, capture_output=True, timeout=60)  # warm the isolated daemon
t = Term()
check("shell started with profile (prompt visible)", t.wait(r"PS [A-Z]:\\.*>", 40))
time.sleep(1.5)

# 1. failure -> suggestion line from the prompt hook; Alt+F inserts it; exit codes are real
t.type("gti status"); t.keys("enter")
ok = t.wait(r"reman: .*-> git status", 25)
t.snap("after failed `gti status`")
check("prompt hook prints a fix suggestion after a failure", ok)
t.keys("alt_f"); time.sleep(0.6)
check("Alt+F inserts the suggested fix into the prompt", t.prompt_line().rstrip().endswith("git status"))
t.keys("esc"); time.sleep(0.4)
d = daemon({"op": "detail", "command": "gti status"})
check("failed PowerShell command recorded as FAIL (real exit code)", d.get("status") == "fail")
t.type("cmd /c exit 3"); t.keys("enter"); time.sleep(1.2)
t.type("echo ok-run"); t.keys("enter"); time.sleep(1.2)
check("native exit code 3 recorded as fail", daemon({"op": "detail", "command": "cmd /c exit 3"}).get("status") == "fail")
check("successful command recorded as ok", daemon({"op": "detail", "command": "echo ok-run"}).get("status") == "ok")

# 1b. Tab: completion still completes; with nothing to complete it opens the finder
t.type("ru"); t.keys("tab"); time.sleep(1.2)
check("Tab still does normal completion (ru -> .\\rust)", "rust" in t.prompt_line() and not finder_running())
t.keys("esc"); time.sleep(0.4)
t.type("zzqq-nothing"); t.keys("tab")
check("Tab with nothing to complete opens the finder seeded with the text", t.wait(r"reman> zzqq-nothing", 12))
t.keys("esc"); time.sleep(0.8); t.keys("esc"); time.sleep(0.4)
r = daemon({"op": "search", "query": "tear down containers", "k": 3})
top = [x["command"] for x in r["results"]]
print("   `tear down containers` top 3:", top)
check("search no longer returns reman's own invocations", not any("reman" in c.lower() for c in top) and any("docker" in c for c in top))
check("unknown-outcome commands have no success rate (not 0%)",
      all(x.get("success_rate") is None for x in r["results"] if x.get("status") == "unknown"))

# 2. UpArrow -> finder scoped to this folder
t.keys("up")
opened = t.wait(r"reman>", 15) and t.wait(r"scope:folder", 5)
time.sleep(1.0)
t.snap("UpArrow finder (empty query, this folder)")
check("UpArrow opens the finder scoped to this folder", opened)
t.type("git st"); time.sleep(1.2)
t.snap("typed `git st` (folder scope)")
b = t.bar()
check("typing filters live (query in bar, results counted)", "git st" in b and re.search(r"\[(hybrid|fuzzy|semantic|manual)\] [1-9]", b) is not None)
t.keys("enter"); time.sleep(1.0)
pl = t.prompt_line()
check("Enter inserts the picked command at the prompt", "git" in pl.split(">", 1)[-1])
print("   prompt now:", pl[:110])
t.keys("esc"); time.sleep(0.4)

# 3. Ctrl+R -> all folders; ranking; chips
t.keys("ctrl_r")
check("Ctrl+R opens the finder over all folders", t.wait(r"scope:all", 15))
t.type("git st"); time.sleep(1.2)
sel_rows = [r for r in t.rows() if r.startswith("> ")]
best = sel_rows[-1] if sel_rows else ""
print("   bar:", t.bar()[:90], "| top row:", best[:80])
t.snap("Ctrl+R `git st`")
check("`git st` ranks `git status` first across folders", "git status" in best)
t.keys("ctrl_u"); time.sleep(0.4)
t.type("docker"); time.sleep(1.0)
t.keys("tab"); time.sleep(0.8)
check("Tab cycles actor filter (all -> you)", "actor:you" in t.text())
t.keys("f2"); time.sleep(0.8)
check("F2 cycles pass filter (all -> ok)", "pass:ok" in t.text())
t.keys("right"); time.sleep(0.8)
check("Right arrow cycles scope (all -> folder)", "scope:folder" in t.text())
t.keys("left"); time.sleep(0.8)
t.snap("Ctrl+R `docker`, actor:you pass:ok")
t.keys("ctrl_t"); time.sleep(1.0)
t.snap("Ctrl+T -> Fixes tab")
check("Ctrl+T switches to Fixes (did-you-mean results)", "typo" in t.text() or "PROVEN" in t.text())
t.keys("ctrl_t"); time.sleep(1.2)
t.snap("Ctrl+T -> Flows tab")
check("Ctrl+T again switches to Flows", " ; " in t.text() or "workflow run" in t.text() or "no matches" in t.text())
t.keys("esc")
deadline = time.time() + 10
while time.time() < deadline and finder_running():
    time.sleep(0.3)
check("Esc closes the finder (process exits)", not finder_running())
time.sleep(0.5)

# 4. forget via Del Del (on the COPY db); leading-space commands are never recorded
t.type(" echo reman-tui-secret-space"); t.keys("enter"); time.sleep(0.8)
t.type("echo reman-tui-forget-me"); t.keys("enter"); time.sleep(1.2)
check("leading-space command was NOT recorded", not daemon({"op": "detail", "command": "echo reman-tui-secret-space"}).get("found"))
t.keys("ctrl_r"); t.wait(r"reman>", 10)
t.type("reman-tui"); time.sleep(1.2)
has = "reman-tui-forget-me" in t.text()
t.keys("del"); time.sleep(0.5)
asked = "press Del again" in t.text()
t.keys("del"); time.sleep(1.2)
t.snap("after Del Del")
check("Del asks to confirm, second Del forgets", has and asked and "forgotten" in t.text()
      and not daemon({"op": "detail", "command": "echo reman-tui-forget-me"}).get("found"))
t.keys("esc"); time.sleep(0.5)

# 5. empty query shows predicted next commands after a repeated sequence
for _ in range(2):
    for c in ("echo step-one-tui", "echo step-two-tui"):
        t.type(c); t.keys("enter"); time.sleep(0.9)
t.type("echo step-one-tui"); t.keys("enter"); time.sleep(1.0)
t.keys("up"); t.wait(r"reman>", 10); time.sleep(1.2)
t.snap("empty query after `echo step-one-tui` (prediction)")
check("empty-query finder predicts the next command (» step-two)", re.search(r"».*echo step-two-tui", t.text()) is not None)
t.keys("esc"); time.sleep(0.5)

t.type("exit"); t.keys("enter")
print("\nTUI RESULT:", "PASS" if all(ok for _, ok in results) else "FAIL " + str([n for n, ok in results if not ok]))
subprocess.run([EXE, "stop"], env=ENV, capture_output=True, timeout=30)
