"""reman's keys next to PSReadLine's own predictions, in a real PowerShell (ConPTY) that loads
`reman init powershell` under each edit mode and prediction view: Alt+F inserts a fix only while
one is waiting, and is otherwise what it was (unbound in Windows mode, ForwardWord in Emacs mode,
which also accepts a word of the inline prediction); with the ListView, Up on typed text moves
through the list, and on an empty line opens the finder. Isolated: own port + a COPY of the db."""
import os, sys, time, shutil, threading, re, subprocess, json, socket
import winpty, pyte

sys.stdout.reconfigure(encoding="utf-8")
ROWS, COLS = 30, 120
EXE = os.environ.get("REMAN_EXE_UNDER_TEST") or os.path.join(os.path.expanduser("~"), ".reman", "bin", "reman.exe")
TMP = os.environ["TEMP"]
DB = os.path.join(TMP, "reman-pskeys.db")
subprocess.run([EXE, "stop"], env=dict(os.environ, REMAN_PORT="8768"), capture_output=True, timeout=30)
time.sleep(0.5)
for ext in ("", "-wal", "-shm"):
    try:
        os.remove(DB + ext)
    except FileNotFoundError:
        pass
shutil.copy(os.path.join(os.path.expanduser("~"), ".reman", "reman.db"), DB)
# reman's settings as a new user has them (yours may move keys: reman settings, Keys)
CFG = os.path.join(TMP, "reman-pskeys-config.json")
with open(CFG, "w") as f:
    f.write("{}")
ENV = dict(os.environ, REMAN_PORT="8768", REMAN_DB=DB, REMAN_CONFIG=CFG, REMAN_SPOOL=os.path.join(TMP, "reman-pskeys-spool.jsonl"))
for k in ("CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT", "AGENT"):
    ENV.pop(k, None)
subprocess.run([EXE, "ping"], env=ENV, capture_output=True, timeout=60)
KEY = {"enter": "\r", "up": "\x1b[A", "esc": "\x1b", "home": "\x1b[H", "alt_f": "\x1bf", "alt_n": "\x1bn", "ctrl_a": "\x01", "ctrl_r": "\x12"}
results = []


def check(name, ok, detail=""):
    results.append(ok)
    print(f"  {'ok  ' if ok else 'FAIL'} {name}" + (f"   [{detail}]" if detail and not ok else ""))


class Term:
    def __init__(self, pre):
        self.screen = pyte.Screen(COLS, ROWS)
        self.stream = pyte.Stream(self.screen)
        self.lock = threading.Lock()
        # what a profile does, with PSReadLine already loaded as it is when a profile runs
        prof = os.path.join(TMP, "reman-pskeys-profile.ps1")
        open(prof, "w", encoding="utf-8-sig").write(
            f"Import-Module PSReadLine\n{pre}\n& '{EXE}' init powershell | Out-String | Invoke-Expression\nClear-Host\n")
        # interactive from the start: under -Command, Windows PowerShell's prompt never uses PSReadLine
        self.p = winpty.PtyProcess.spawn("powershell.exe -NoLogo -NoProfile", env=ENV, dimensions=(ROWS, COLS))
        threading.Thread(target=self._pump, daemon=True).start()
        self.wait(r"PS [A-Z]:\\.*>", 40)
        time.sleep(0.8)
        self.type(f". '{prof}'"); self.keys("enter")
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

    def keys(self, *ks):
        for k in ks:
            self.p.write(KEY.get(k, k))
            time.sleep(0.2)

    def type(self, s):
        for ch in s:
            self.p.write(ch)
            time.sleep(0.03)

    def run(self, cmd, wait=1.5):
        self.type(cmd); self.keys("enter"); time.sleep(wait)

    def prompt_line(self):
        lines = [l for l in self.text().splitlines() if re.match(r"PS [A-Z]:\\", l)]
        return lines[-1] if lines else ""

    def close(self):
        try:
            self.p.terminate(force=True)
        except Exception:
            pass


print("=== Windows edit mode (the default) ===")
t = Term("$null")
check("prompt", t.wait(r"PS [A-Z]:\\.*>", 40)); time.sleep(1.5)
t.run("gti status", 0.5)
check("fix suggested after a failure", t.wait(r"reman: .*-> git status", 25))
check("...with what it changes", "(gti → git; Alt+F inserts)" in t.text(), t.text()[-300:])
t.keys("alt_f"); time.sleep(0.6)
check("Alt+F inserts the fix", t.prompt_line().rstrip().endswith("git status"), t.prompt_line())
t.keys("esc"); time.sleep(0.4)
t.run("echo next")
t.type("echo one two"); t.keys("home", "alt_f"); t.type("X"); time.sleep(0.6)
check("Alt+F with no fix: unbound, as without reman (old fix not inserted)", t.prompt_line().rstrip().endswith("Xecho one two"), t.prompt_line())
t.keys("esc"); time.sleep(0.4)

# Enter holds a command that keeps failing here, once, only when failing costs something (slow,
# or it changes things); a command that fails in a blink just runs
stamp = int(time.time())
bad = f"zzfail-{stamp}"
t.run(bad); t.run(bad); time.sleep(1.0)
t.run("Clear-Host")
t.keys("esc"); t.type(bad); t.keys("enter"); time.sleep(2.0)
check("a command that fails in a blink is not held, however often it failed",
      "Enter again runs it anyway" not in t.text() and "is not recognized" in t.text(), t.text()[-400:])
slow = f'powershell -NoProfile -Command "Start-Sleep 11; exit 3" # zzslow-{stamp}'
t.run(slow, 14); t.run(slow, 14)
t.run("Clear-Host")
t.type(slow); t.keys("enter")
held = t.wait(r"reman: this failed all 2 times it ran here, after about 11s each time", 8)
time.sleep(0.8)
check("a slow one is held, with what failing costs", held and "Enter again runs it anyway" in t.text(), t.text()[-600:])
check("...keeping the line on a fresh prompt", t.prompt_line().rstrip().endswith(f"zzslow-{stamp}"), t.prompt_line())
t.keys("enter"); time.sleep(13)
check("Enter again runs it", t.text().count(f"zzslow-{stamp}") >= 2 and "Enter again runs it anyway" in t.text(), t.text()[-400:])
# a fast failure that changes things is held too, with what worked instead
rm = f"Remove-Item zz-missing-{stamp}"
# fails (no such file); then the file is made and `-Force` removes it: what worked instead
t.run(rm); t.run(f"New-Item zz-missing-{stamp} -ItemType File | Out-Null"); t.run(f"{rm} -Force"); t.run(rm); time.sleep(1.0)
t.run("Clear-Host")
t.type(rm); t.keys("enter")
held = t.wait(r"worked instead -> " + re.escape(f"{rm} -Force"), 8)
check("a command that changes things is held, offering what worked instead (and what it changes)",
      held and "(adds -Force; Alt+F inserts)" in t.text(), t.text()[-600:])
t.keys("alt_f"); time.sleep(0.6)
check("...and Alt+F inserts it", t.prompt_line().rstrip().endswith(f"{rm} -Force"), t.prompt_line())
t.keys("esc"); time.sleep(0.4)
# Alt+N on an empty prompt: what usually comes next here, not run; again: another idea
na, nb = f"echo nx-a-{stamp}", f"echo nx-b-{stamp}"
for _ in range(3):
    t.run(na, 1.0); t.run(nb, 1.0)
t.run(na, 1.5)
t.run("Clear-Host")
t.keys("alt_n"); time.sleep(1.5)
check("Alt+N on an empty prompt puts what usually comes next here", t.prompt_line().rstrip().endswith(nb), t.prompt_line())
check("...with why, above the prompt", f"reman: after `{na}` (3x)" in t.text(), t.text()[-400:])
t.keys("alt_n"); time.sleep(1.2)
after = t.prompt_line().split("> ", 1)[-1].strip()
check("Alt+N again: another idea", after != "" and after != nb, t.prompt_line())
t.keys("esc"); time.sleep(0.3)
t.type("abc"); t.keys("alt_n"); time.sleep(0.8)
check("Alt+N with other text typed leaves it alone", t.prompt_line().rstrip().endswith("abc"), t.prompt_line())
t.keys("esc"); time.sleep(0.4)
# a command with a blank: Enter puts it on the prompt with the cursor in the blank
t.run(f'Test-Path "zzone-{stamp}"', 1.0); t.run(f'Test-Path "zztwo-{stamp}"', 1.0); time.sleep(1.0)
t.run("Clear-Host")
t.keys("ctrl_r")
opened = t.wait(r"Recall  Fixes  Flows", 15)
t.type("test-path"); time.sleep(2.0)
check("the finder shows the variants as one command with a blank", opened and 'Test-Path "‹text›"' in t.text(), t.text()[-800:])
t.keys("enter"); time.sleep(1.5)
t.type("x"); time.sleep(0.6)
check("Enter: the cursor lands in the blank", t.prompt_line().rstrip().endswith('Test-Path "x"'), t.prompt_line())
t.keys("esc"); time.sleep(0.4)
# rcd: go to the folder by what you did there, or by its name
proj = f"zzproj-{stamp}"
t.run(f"New-Item -ItemType Directory {proj} | Out-Null; Push-Location {proj}", 1.5)
t.run(f"echo built-{stamp}", 1.0)
t.run("Pop-Location", 1.0)
t.run(f"rcd {proj}", 3.0)
check("rcd <name> goes to that folder", f"\\{proj}>" in t.prompt_line(), t.prompt_line())
t.run("Set-Location ..", 1.0)
t.run(f"Remove-Item {proj}", 1.0)
t.close()

print("=== Emacs edit mode + inline predictions ===")
t = Term("Set-PSReadLineOption -EditMode Emacs -PredictionSource History -PredictionViewStyle InlineView")
check("prompt", t.wait(r"PS [A-Z]:\\.*>", 40)); time.sleep(1.5)
t.run("gti status", 0.5)
check("fix suggested after a failure", t.wait(r"reman: .*-> git status", 25))
t.keys("alt_f"); time.sleep(0.6)
check("Alt+F inserts the fix", t.prompt_line().rstrip().endswith("git status"), t.prompt_line())
t.keys("\x05", "\x15"); time.sleep(0.4)   # end, kill line
t.run("echo alpha-zz beta gamma")
t.type("echo one two"); t.keys("ctrl_a", "alt_f"); t.type("X"); time.sleep(0.6)
check("Alt+F with no fix: ForwardWord", "echoX one two" in t.prompt_line(), t.prompt_line())
t.keys("\x05", "\x15"); time.sleep(0.4)
t.type("echo alp"); time.sleep(0.6); t.keys("alt_f"); t.type("Z"); time.sleep(0.6)
line = t.prompt_line()
check("Alt+F accepts one word of the inline prediction", "echo alphaZ" in line or "echo alpha-Z" in line or "echo alpha-zzZ" in line, line)
t.keys("\x05", "\x15"); time.sleep(0.4)
t.close()

print("=== ListView predictions ===")
t = Term("Set-PSReadLineOption -PredictionSource History -PredictionViewStyle ListView")
check("prompt", t.wait(r"PS [A-Z]:\\.*>", 40)); time.sleep(1.5)
t.run("echo listview-only-zq")
t.type("echo listview-o"); time.sleep(0.8)
check("list shows the history match", "[History]" in t.text(), t.text()[-400:])
t.keys("up"); time.sleep(1.2)
check("Up with text does not open reman's finder", "Recall  Fixes  Flows" not in t.text(), t.text()[-300:])
check("Up selects from the list", "echo listview-only-zq" in t.prompt_line(), t.prompt_line())
t.keys("\x03"); time.sleep(0.8)
check("line cleared", t.prompt_line().rstrip().endswith(">"), t.prompt_line())
t.keys("up")
check("Up on an empty line still opens reman's finder", t.wait(r"Recall  Fixes  Flows", 15), t.text()[-300:])
t.keys("esc"); time.sleep(0.8)
t.close()

print("=== Inline predictions: Up with text still opens the finder ===")
t = Term("Set-PSReadLineOption -PredictionSource History -PredictionViewStyle InlineView")
check("prompt", t.wait(r"PS [A-Z]:\\.*>", 40)); time.sleep(1.5)
t.type("docker"); t.keys("up")
check("Up with text opens reman's finder", t.wait(r"Recall  Fixes  Flows", 15), t.text()[-300:])
t.keys("esc"); time.sleep(0.8)
t.keys("\x03"); time.sleep(0.8)
t.run("Clear-Host")   # the finder's last frame stays in the emulator's buffer
# Tab with nothing to complete takes the grey prediction; with no prediction either, the finder
t.run("echo zztab-alpha beta")
t.type("echo zztab"); time.sleep(0.8); t.keys("\t"); time.sleep(1.2)
check("Tab with nothing to complete accepts the grey prediction",
      t.prompt_line().rstrip().endswith("echo zztab-alpha beta") and "Recall  Fixes  Flows" not in t.text(), t.prompt_line())
t.keys("\x03"); time.sleep(0.8)
t.run("cd .")
t.type("cd"); time.sleep(0.8); t.keys("\t"); time.sleep(1.2)
check("`cd` + Tab takes the grey `cd .`, no finder",
      t.prompt_line().rstrip().endswith("cd .") and "Recall  Fixes  Flows" not in t.text(), t.prompt_line())
t.keys("\x03"); time.sleep(0.8)
t.type("zzqqxx-nothing"); time.sleep(0.8); t.keys("\t")
check("Tab with nothing to complete and no prediction opens the finder", t.wait(r"Recall  Fixes  Flows", 15), t.text()[-300:])
t.keys("esc"); time.sleep(0.8)
t.close()

subprocess.run([EXE, "stop"], env=ENV, capture_output=True, timeout=30)
print(f"RESULT: {'PASS' if all(results) else 'FAIL'} ({sum(results)}/{len(results)})")
