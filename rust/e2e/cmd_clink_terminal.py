"""Drive a REAL Command Prompt (cmd.exe with Clink, which loads reman's integration from its profile
folder) in ConPTY: capture with real %ERRORLEVEL%, the fix suggestion + Alt+F, text cmd can't
quote, leading-space privacy, the finder on Up / Ctrl+R, and Tab completion for reman.
Isolated: own port + a COPY of the db. Needs Clink + `reman setup` (writes reman.lua into
Clink's profile folder)."""
import os, sys, time, shutil, threading, json, socket, subprocess
import winpty, pyte

sys.stdout.reconfigure(encoding="utf-8")
# Clink loads into Command Prompt through its AutoRun (`clink inject`); without it there's no Clink
# to test (the plain Command Prompt is cmd_terminal.py's)
_autorun = subprocess.run(["reg", "query", r"HKCU\Software\Microsoft\Command Processor", "/v", "AutoRun"], capture_output=True, text=True).stdout
if "clink" not in _autorun.lower():
    print("Clink isn't hooked into Command Prompt here (no `clink inject` in its AutoRun): nothing to test. See cmd_terminal.py.")
    print("CMD RESULT: SKIPPED")
    sys.exit(0)
ROWS, COLS = 30, 120
TMP = os.environ["TEMP"]
DB = os.path.join(TMP, "reman-cmd.db")
PORT = "8766"
EXE = os.environ.get("REMAN_EXE_UNDER_TEST") or os.path.join(os.path.expanduser("~"), ".reman", "bin", "reman.exe")
ENV = dict(os.environ, REMAN_PORT=PORT, REMAN_DB=DB, REMAN_SPOOL=os.path.join(TMP, "reman-cmd-spool.jsonl"))
for k in ("CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT", "AGENT", "REMAN_SESSION"):
    ENV.pop(k, None)
subprocess.run([EXE, "stop"], env=ENV, capture_output=True, timeout=30)
time.sleep(0.5)
for ext in ("", "-wal", "-shm"):
    try:
        os.remove(DB + ext)
    except FileNotFoundError:
        pass
shutil.copy(os.path.join(os.path.expanduser("~"), ".reman", "reman.db"), DB)
subprocess.run([EXE, "ping"], env=ENV, capture_output=True, timeout=60)

KEY = {"enter": "\r", "up": "\x1b[A", "esc": "\x1b", "tab": "\t", "ctrl_r": "\x12", "alt_f": "\x1bf", "esc_line": "\x1b"}


class Term:
    def __init__(self):
        self.screen = pyte.Screen(COLS, ROWS)
        self.stream = pyte.Stream(self.screen)
        self.lock = threading.Lock()
        self.p = winpty.PtyProcess.spawn("cmd.exe", env=ENV, dimensions=(ROWS, COLS), cwd=os.path.expanduser("~"))
        threading.Thread(target=self._pump, daemon=True).start()

    def _pump(self):
        while True:
            try:
                d = self.p.read(65536)
            except EOFError:
                return
            if "\x1b[c" in d:
                # ConPTY waits for the terminal's DA1 answer before rendering; pyte doesn't send one
                self.p.write("\x1b[?61;4;6;7;14;21;22;23;24;28;32;42c")
            with self.lock:
                self.stream.feed(d)

    def text(self):
        with self.lock:
            return "\n".join(l.rstrip() for l in self.screen.display)

    def wait(self, pattern, timeout=15):
        t = time.time()
        while time.time() - t < timeout:
            if pattern in self.text():
                return True
            time.sleep(0.1)
        return False

    def keys(self, *ks, gap=0.2):
        for k in ks:
            self.p.write(KEY.get(k, k))
            time.sleep(gap)

    def type(self, s):
        for ch in s:
            self.p.write(ch)
            time.sleep(0.03)

    def run(self, cmd, wait=1.5):
        self.type(cmd)
        self.keys("enter")
        time.sleep(wait)

    def prompt_line(self):
        lines = [l for l in self.text().splitlines() if l.rstrip().startswith("C:\\") and ">" in l]
        return lines[-1] if lines else ""

    def snap(self, title):
        print(f"\n----- {title} " + "-" * max(0, COLS - 8 - len(title)))
        body = self.text().splitlines()
        while body and not body[-1].strip():
            body.pop()
        print("\n".join(body[-ROWS:]))


def daemon(req):
    s = socket.create_connection(("127.0.0.1", int(PORT)), timeout=10)
    s.sendall((json.dumps(req) + "\n").encode())
    d = b""
    while not d.endswith(b"\n"):
        d += s.recv(1 << 20)
    return json.loads(d)


def status(cmd):
    return daemon({"op": "detail", "command": cmd}).get("status")


results = []


def check(name, ok):
    results.append((name, ok))
    print(f"  {'ok  ' if ok else 'FAIL'} {name}")


t = Term()
check("cmd.exe started with Clink", t.wait(">", 20))
time.sleep(2.5)

t.run("gti status", 3)
t.snap("after `gti status`")
check("unknown command recorded as FAIL (errorlevel 9009)", status("gti status") == "fail")
check("a fix suggestion is printed", "reman:" in t.text() and "git status" in t.text())
t.keys("alt_f")
time.sleep(0.6)
check("Alt+F puts the fix on the command line", t.prompt_line().rstrip().endswith("git status"))
t.keys("esc")
time.sleep(0.3)

t.run("dir nope-zz-cmd", 3)
check("a failing builtin (dir of a missing path) recorded as FAIL", status("dir nope-zz-cmd") == "fail")
t.run("if errorlevel 1 (echo el-kept) else (echo el-lost)", 2)
check("`if errorlevel` still sees the previous command's result", "el-kept" in t.text())
odd = 'echo 50% "quoted & piped | text" ^& done'
t.run(odd, 3)
check("a success is recorded (through the spool) with its text intact", status(odd) == "ok")
t.run(" echo secret-cmd-space", 2.5)
check("a leading-space command is not recorded", not daemon({"op": "detail", "command": "echo secret-cmd-space"}).get("found"))
d = daemon({"op": "detail", "command": odd})
check("the folder is recorded", (d.get("cwd") or "").lower() == os.path.expanduser("~").lower())

t.keys("up")
check("Up opens the finder scoped to this folder", t.wait("in this folder ←→", 15))
time.sleep(1.0)
t.snap("Up: the finder")
t.keys("esc")
time.sleep(1.0)
t.keys("ctrl_r")
check("Ctrl+R opens the finder over all folders", t.wait("everywhere ←→", 15))
t.type("git status")
time.sleep(1.5)
t.keys("enter")
time.sleep(1.2)
check("Enter puts the pick on the command line", "git" in t.prompt_line().split(">", 1)[-1])
t.keys("esc")
time.sleep(0.3)

t.type("reman con")
t.keys("tab")
time.sleep(2.5)
check("Tab completes reman's own commands (reman con -> reman connect)", "reman connect" in t.prompt_line())
t.keys("esc")
time.sleep(0.3)
t.type("reman init c")
t.keys("tab")
time.sleep(2.5)
t.snap("reman init c + Tab")
check("Tab completes values too (reman init c -> cmd)", "reman init cmd" in t.prompt_line())
t.keys("esc")

t.type("exit")
t.keys("enter")
subprocess.run([EXE, "stop"], env=ENV, capture_output=True, timeout=30)
print("\nCMD RESULT:", "PASS" if all(ok for _, ok in results) else "FAIL " + str([n for n, ok in results if not ok]))
