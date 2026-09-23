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
       "ctrl_g": "\x07", "ctrl_p": "\x10", "ctrl_u": "\x15", "alt_f": "\x1bf", "f1": "\x1bOP", "f3": "\x1bOR",
       "shift_tab": "\x1b[Z", "alt_1": "\x1b1", "alt_3": "\x1b3"}


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
            if "\x1b[c" in data:
                # ConPTY asks the terminal to identify itself (DA1) and holds rendering until it
                # hears back; Windows Terminal answers, pyte doesn't
                self.p.write("\x1b[?61;4;6;7;14;21;22;23;24;28;32;42c")
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
        """The finder's filter line + the query line under it (bottom-up: the query is last)."""
        lines = self.text().splitlines()
        i = next((k for k, l in enumerate(lines) if l.startswith((" › ", " ⇢ "))), None)
        return "" if i is None else (lines[i - 1] if i > 0 else "") + "\n" + lines[i]

    def selected(self):
        s = [l for l in self.text().splitlines() if l.startswith(" ▌")]
        return s[0] if s else ""

    def snap(self, title):
        print(f"\n----- {title} " + "-" * max(0, COLS - 8 - len(title)))
        body = self.text().splitlines()
        while body and not body[-1].strip():
            body.pop()
        print("\n".join(body[-ROWS:]))


FINDER = r"Recall  Fixes  Flows"


def finder_running():
    # only the finder opened by OUR test shell - the user may have one open in a real terminal
    out = subprocess.run(["powershell", "-NoProfile", "-Command",
                          f"@(Get-CimInstance Win32_Process -Filter \"Name='reman.exe' AND ParentProcessId={t.p.pid}\" | Where-Object {{ $_.CommandLine -match ' find ' }}).Count"],
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
check("Tab with nothing to complete opens the finder seeded with the text", t.wait(r" › zzqq-nothing", 12))
t.keys("esc"); time.sleep(0.8); t.keys("esc"); time.sleep(0.4)
r = daemon({"op": "search", "query": "tear down containers", "k": 3})
top = [x["command"] for x in r["results"]]
print("   `tear down containers` top 3:", top)
check("search no longer returns reman's own invocations", not any("reman" in c.lower() for c in top) and any("docker" in c for c in top))
check("unknown-outcome commands have no success rate (not 0%)",
      all(x.get("success_rate") is None for x in r["results"] if x.get("status") == "unknown"))

# 2. UpArrow -> finder scoped to this folder
t.type("gti status"); t.keys("enter"); time.sleep(1.5)
t.keys("up")
opened = t.wait(FINDER, 15) and t.wait(r"in this folder ←→", 5)
time.sleep(1.2)
t.snap("UpArrow finder right after `gti status` failed")
check("UpArrow opens the finder scoped to this folder", opened)
check("right after a failure the finder leads with its fix",
      "last command failed: gti status" in t.text() and "git status" in t.selected())
t.type("git st"); time.sleep(1.2)
t.snap("typed `git st` (folder scope)")
b = t.bar()
check("typing filters live (query in bar, results counted)", "git st" in b and re.search(r"\d+ (results?|of \d+)", b) is not None)
t.keys("ctrl_u"); time.sleep(1.2)
check("clearing the query redraws the empty state", "describe it in words" in t.bar() and "last command failed" in t.text())
t.type("git st"); time.sleep(1.5)
t.snap("retyped `git st` before Enter")
t.keys("enter"); time.sleep(1.0)
pl = t.prompt_line()
check("Enter inserts the picked command at the prompt", "git" in pl.split(">", 1)[-1])
print("   prompt now:", pl[:110])
t.keys("esc"); time.sleep(0.4)

# 3. Ctrl+R -> all folders; ranking; chips
t.keys("ctrl_r")
check("Ctrl+R opens the finder over all folders", t.wait(r"everywhere ←→", 15))
t.type("git st"); time.sleep(1.2)
best = t.selected()
print("   bar:", t.bar().splitlines()[0][:90], "| selected:", best[:80])
t.snap("Ctrl+R `git st`")
check("`git st` ranks `git status` first across folders", "git status" in best)
t.keys("ctrl_u"); time.sleep(0.4)
t.type("docker"); time.sleep(1.0)
t.keys("f3"); time.sleep(0.8)
check("F3 cycles who (anyone -> you)", "by you F3" in t.text())
t.keys("f2"); time.sleep(0.8)
check("F2 cycles outcome filter (any -> worked)", "worked F2" in t.text())
t.keys("right"); time.sleep(0.8)
check("Right arrow cycles scope (everywhere -> this folder first)", "this folder first ←→" in t.text())
t.keys("left"); time.sleep(0.8)
t.snap("Ctrl+R `docker`, by you, worked")
t.keys("tab"); time.sleep(1.2)
t.snap("Tab -> Fixes tab (its own, empty query)")
check("Tab switches to Fixes, with its own query", "commands that failed" in t.text() or "No failed commands" in t.text())
t.keys("tab"); time.sleep(1.2)
t.snap("Tab -> Flows tab")
check("Tab again switches to Flows", "sequences you repeat" in t.text() or "No repeated" in t.text())
t.keys("shift_tab"); time.sleep(1.0)
check("Shift+Tab goes back to Fixes", "type the command that failed" in t.bar())
t.keys("alt_1"); time.sleep(1.2)
check("Alt+1 jumps to Recall and restores its query", "› docker" in t.bar())
t.keys("f1"); time.sleep(0.6)
check("F1 shows every key in a framed box", "┌ keys" in t.text() and "any key closes" in t.text())
t.keys("x"); time.sleep(0.5)
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
t.keys("ctrl_r"); t.wait(FINDER, 10)
t.type("reman-tui"); time.sleep(1.2)
has = "reman-tui-forget-me" in t.text()
t.keys("del"); time.sleep(0.5)
asked = "Del again forgets" in t.text()
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
t.keys("up"); t.wait(FINDER, 10); time.sleep(1.2)
t.snap("empty query after `echo step-one-tui` (prediction)")
check("empty-query finder predicts the next command (» step-two)", re.search(r"».*echo step-two-tui", t.text()) is not None)
t.keys("esc"); time.sleep(0.5)

# 6. walk through a flow: Flows tab -> open it -> Enter puts step 1 on the prompt; after it
#    runs, the next ↑ offers step 2 first
t.keys("up"); t.wait(FINDER, 10); time.sleep(0.8)
t.keys("alt_3"); time.sleep(1.2)
t.type("step-one"); time.sleep(0.8)
t.snap("Flows filtered to `step-one`")
t.keys("enter"); time.sleep(1.0)
t.snap("the flow opened: its steps")
check("Enter on a flow opens its steps", "steps · ran together" in t.text() and "1. echo step-one-tui" in t.selected())
t.keys("enter"); time.sleep(1.0)
check("Enter on step 1 puts it on the prompt", t.prompt_line().rstrip().endswith("echo step-one-tui"))
t.keys("enter"); time.sleep(1.2)
t.keys("up"); t.wait(FINDER, 10); time.sleep(1.2)
t.snap("↑ after running step 1")
check("after step 1 ran, ↑ offers step 2 first (flow in progress)", "flow in progress" in t.text() and "echo step-two-tui" in t.selected())
t.keys("enter"); time.sleep(1.0)
check("Enter inserts step 2", t.prompt_line().rstrip().endswith("echo step-two-tui"))
t.keys("esc"); time.sleep(0.4)

# 7. Tab completion for reman itself: menu with descriptions, live values
t.type("reman con"); t.keys("tab"); time.sleep(2.0)
check("Tab completes a subcommand (reman con -> reman connect)", t.prompt_line().rstrip().endswith("reman connect"))
t.keys("esc"); time.sleep(0.4)
t.type("reman init fi"); t.keys("tab"); time.sleep(2.0)
check("Tab completes a value (reman init fi -> fish)", t.prompt_line().rstrip().endswith("reman init fish"))
t.keys("esc"); time.sleep(0.4)
t.type("reman connect "); t.keys("tab"); time.sleep(2.5)
t.snap("reman connect <Tab> (menu)")
check("Tab on `reman connect ` lists agents with their status", "claude-code" in t.text() and "windsurf" in t.text())
t.keys("esc"); time.sleep(0.4); t.keys("esc"); time.sleep(0.4)
t.type("reman forget ok-ru"); t.keys("tab"); time.sleep(2.0)
check("Tab offers your own commands, quoted (reman forget ok-ru -> 'echo ok-run')", t.prompt_line().rstrip().endswith("reman forget 'echo ok-run'"))
t.keys("esc"); time.sleep(0.4)

t.type("exit"); t.keys("enter")
print("\nTUI RESULT:", "PASS" if all(ok for _, ok in results) else "FAIL " + str([n for n, ok in results if not ok]))
subprocess.run([EXE, "stop"], env=ENV, capture_output=True, timeout=30)
