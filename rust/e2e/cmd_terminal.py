"""Drive a REAL Command Prompt (plain cmd.exe, no add-ons) in ConPTY. `reman setup` makes cmd load
two DOSKEY macros at start (AutoRun): `h` opens the finder for this folder, `hh` for everywhere,
and the pick is typed onto the next prompt - ready to edit, not run.
Isolated: own port + a COPY of the db."""
import os, sys, time, shutil, threading, json, socket, subprocess
import winpty, pyte

sys.stdout.reconfigure(encoding="utf-8")
ROWS, COLS = 30, 120
TMP = os.environ["TEMP"]
DB = os.path.join(TMP, "reman-cmd.db")
PORT = "8766"
EXE = os.path.join(os.path.expanduser("~"), ".reman", "bin", "reman.exe")
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
HOME = os.path.expanduser("~")
ODD = 'echo 50% "quoted & piped | text" ^& done-cmdtest'


def daemon(req):
    s = socket.create_connection(("127.0.0.1", int(PORT)), timeout=10)
    s.sendall((json.dumps(req) + "\n").encode())
    d = b""
    while not d.endswith(b"\n"):
        d += s.recv(1 << 20)
    return json.loads(d)


# a command full of characters cmd can't quote, run in the folder the test shell starts in
daemon({"op": "ingest", "command": ODD, "exit": 0, "cwd": HOME, "session": "cmdtest", "actor": "human"})


class Term:
    def __init__(self):
        self.screen = pyte.Screen(COLS, ROWS)
        self.stream = pyte.Stream(self.screen)
        self.lock = threading.Lock()
        self.p = winpty.PtyProcess.spawn("cmd.exe", env=ENV, dimensions=(ROWS, COLS), cwd=HOME)
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
            self.p.write({"enter": "\r", "esc": "\x1b"}.get(k, k))
            time.sleep(gap)

    def type(self, s):
        for ch in s:
            self.p.write(ch)
            time.sleep(0.03)

    def prompt_lines(self):
        return [l for l in self.text().splitlines() if l.startswith(HOME + ">")]

    def snap(self, title):
        print(f"\n----- {title} " + "-" * max(0, COLS - 8 - len(title)))
        body = self.text().splitlines()
        while body and not body[-1].strip():
            body.pop()
        print("\n".join(body[-ROWS:]))


results = []


def check(name, ok):
    results.append((name, ok))
    print(f"  {'ok  ' if ok else 'FAIL'} {name}")


t = Term()
check("cmd.exe started", t.wait(HOME + ">", 20))
time.sleep(1.0)
check("no startup error from AutoRun", "is not recognized" not in t.text())
t.type("doskey /macros")
t.keys("enter")
time.sleep(1.5)
check("the h / hh macros are loaded", "h=" in t.text() and "hh=" in t.text())
t.type("cls")
t.keys("enter")
time.sleep(1.0)

# h -> finder for this folder; the pick lands on the prompt, not run
t.type("h")
t.keys("enter")
check("`h` opens the finder scoped to this folder", t.wait("in this folder ←→", 15))
time.sleep(1.0)
t.type("50%")
time.sleep(1.5)
t.snap("h, then typed 50%")
t.keys("enter")
time.sleep(1.5)
# (the emulator here has no alternate screen, so check by behaviour, not by where text is drawn)
OUT = '50% "quoted & piped | text" & done-cmdtest'   # what that echo prints: ^& becomes &
check("the pick waits on the prompt - not run yet", OUT not in t.text())
t.keys("enter")
time.sleep(1.5)
t.snap("after pressing Enter on the typed pick")
check("Enter runs exactly the picked text (quotes, &, |, %, ^ intact)", OUT in t.text())
t.type("cls")
t.keys("enter")
time.sleep(1.0)
t.type("h")
t.keys("enter")
t.wait("in this folder ←→", 15)
time.sleep(1.0)
t.type("50%")
time.sleep(1.5)
t.keys("enter")
time.sleep(1.5)
t.keys("esc")
time.sleep(0.4)
t.type("echo esc-cleared")
t.keys("enter")
time.sleep(1.5)
t.snap("after Esc + echo esc-cleared")
# output lines can land on leftover finder rows here, so match where a line starts
check("Esc clears the typed pick like any typed text",
      any(l.startswith("esc-cleared") for l in t.text().splitlines()) and "done-cmdtestecho" not in t.text() and OUT not in t.text())
t.type("cls")
t.keys("enter")
time.sleep(1.0)

# hh <words> -> finder everywhere, seeded with the words
t.type("hh docker compose")
t.keys("enter")
check("`hh` opens the finder over all folders", t.wait("everywhere ←→", 15))
time.sleep(1.0)
check("words after hh seed the query", "› docker compose" in t.text())
t.keys("esc")
time.sleep(1.2)
t.type("echo nothing-typed")
t.keys("enter")
time.sleep(1.5)
t.snap("after finder Esc + echo nothing-typed")
check("Esc in the finder types nothing", any(l.startswith("nothing-typed") for l in t.text().splitlines()))

t.type("exit")
t.keys("enter")
subprocess.run([EXE, "stop"], env=ENV, capture_output=True, timeout=30)
print("\nCMD RESULT:", "PASS" if all(ok for _, ok in results) else "FAIL " + str([n for n, ok in results if not ok]))
