"""bash / zsh / fish in REAL ConPTY terminals, each loading `reman init <shell>`, against an
isolated daemon (port 8768) on a COPY of the db. Checks capture, exit codes, suggestion + Alt-F,
leading-space privacy, the finder (Ctrl-R / Up), cross-shell folder identity, and hook latency."""
import os, sys, time, shutil, threading, re, subprocess, json, socket, tempfile
import winpty, pyte

sys.stdout.reconfigure(encoding="utf-8")
ROWS, COLS = 30, 130
TMP = os.environ["TEMP"]
DB = os.path.join(TMP, "reman-shells.db")
BIN = os.path.join(os.path.expanduser("~"), ".reman", "bin")
EXE = os.path.join(BIN, "reman.exe")
EXE_FWD = EXE.replace("\\", "/")
PORT = "8768"
subprocess.run([EXE, "stop"], env=dict(os.environ, REMAN_PORT=PORT), capture_output=True, timeout=30)
time.sleep(0.5)
for ext in ("", "-wal", "-shm"):
    try:
        os.remove(DB + ext)
    except FileNotFoundError:
        pass
shutil.copy(os.path.join(os.path.expanduser("~"), ".reman", "reman.db"), DB)
BASE_ENV = dict(os.environ, REMAN_PORT=PORT, REMAN_DB=DB, REMAN_SPOOL=os.path.join(TMP, "reman-shells-spool.jsonl"), TERM="xterm-256color")
for k in ("CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT", "AGENT"):
    BASE_ENV.pop(k, None)
subprocess.run([EXE, "ping"], env=BASE_ENV, capture_output=True, timeout=60)

KEY = {"enter": "\r", "up": "\x1b[A", "esc": "\x1b", "ctrl_r": "\x12", "ctrl_u": "\x15", "alt_f": "\x1bf", "ctrl_c": "\x03"}
CWD = r"C:\Users\rdars\reman"


def daemon(req):
    s = socket.create_connection(("127.0.0.1", int(PORT)), timeout=10)
    s.sendall((json.dumps(req) + "\n").encode())
    d = b""
    while not d.endswith(b"\n"):
        d += s.recv(1 << 20)
    return json.loads(d)


class Term:
    def __init__(self, argv, env):
        self.screen = pyte.Screen(COLS, ROWS)
        self.stream = pyte.Stream(self.screen)
        self.lock = threading.Lock()
        self.p = winpty.PtyProcess.spawn(argv, env=env, cwd=CWD, dimensions=(ROWS, COLS))
        threading.Thread(target=self._pump, daemon=True).start()

    def _pump(self):
        while True:
            try:
                data = self.p.read(65536)
            except EOFError:
                return
            if "\x1b[c" in data:
                # ConPTY waits for the terminal's DA1 answer before rendering; pyte doesn't send one
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
            time.sleep(0.15)

    def type(self, s):
        for ch in s:
            self.p.write(ch)
            time.sleep(0.03)

    def run(self, cmd, wait=1.2):
        self.type(cmd)
        self.keys("enter")
        time.sleep(wait)

    def last_prompt(self):
        lines = [l for l in self.text().splitlines() if "RMN>" in l]
        return lines[-1] if lines else ""


def shell_setup(name):
    d = tempfile.mkdtemp(prefix=f"reman-{name}-")
    env = dict(BASE_ENV)
    if name == "bash":
        rc = os.path.join(d, "bashrc")
        open(rc, "w", newline="\n").write(f'PS1="RMN> "\neval "$("{EXE_FWD}" init bash)"\n')
        return [r"C:\Program Files\Git\usr\bin\bash.exe", "--noprofile", "--rcfile", rc.replace("\\", "/"), "-i"], env
    if name == "zsh":
        open(os.path.join(d, ".zshrc"), "w", newline="\n").write(f'PROMPT="RMN> "\neval "$("{EXE_FWD}" init zsh)"\n')
        env.update(ZDOTDIR=d.replace("\\", "/"), MSYSTEM="MSYS", PATH=r"D:\msys64\usr\bin;" + env["PATH"])
        return [r"D:\msys64\usr\bin\zsh.exe", "-i"], env
    if name == "fish":
        os.makedirs(os.path.join(d, "fish"))
        open(os.path.join(d, "fish", "config.fish"), "w", newline="\n").write(
            f'function fish_prompt; echo -n "RMN> "; end\nfunction fish_greeting; end\n"{EXE_FWD}" init fish | source\n')
        env.update(XDG_CONFIG_HOME=d.replace("\\", "/"), MSYSTEM="MSYS", PATH=r"D:\msys64\usr\bin;" + env["PATH"])
        return [r"D:\msys64\usr\bin\fish.exe", "-i"], env


summary = {}
for sh in ("bash", "zsh", "fish"):
    print(f"\n=========== {sh} ===========")
    res = []

    def check(name, ok, detail=""):
        res.append(ok)
        print(f"  {'ok  ' if ok else 'FAIL'} {name}" + (f"   [{detail}]" if detail and not ok else ""))

    argv, env = shell_setup(sh)
    t = Term(argv, env)
    check("shell starts with reman integration", t.wait(r"RMN>", 40), t.text()[-300:])
    time.sleep(1.0)
    tag = f"{sh}-{int(time.time())}"
    t.run(f"echo ok-{tag}", 2.5)   # fish's spool path lands within the 1s drain tick
    d = daemon({"op": "detail", "command": f"echo ok-{tag}"})
    check("success captured: status ok, human, this folder", d.get("found") and d.get("status") == "ok" and d.get("actor") == "human"
          and "reman" in (d.get("cwd") or "").lower(), d)
    t.run("gti status", 2.0)
    d = daemon({"op": "detail", "command": "gti status"})
    check("failure captured with real exit code", d.get("status") in ("fail", "mixed"), d)
    check("suggestion printed after the failure", re.search(r"reman: .*(->|→) ?git status", t.text()) is not None, t.text()[-400:])
    t.keys("alt_f"); time.sleep(0.8)
    check("Alt-F inserts the fix", t.last_prompt().rstrip().endswith("git status"), t.last_prompt())
    t.keys("ctrl_u"); time.sleep(0.3)
    t.run(f" echo secret-{tag}")
    check("leading-space command not recorded", not daemon({"op": "detail", "command": f"echo secret-{tag}"}).get("found"))
    # finder
    t.keys("ctrl_r")
    opened = t.wait(r"Recall  Fixes  Flows", 15) and t.wait(r"everywhere ←→", 5)
    check("Ctrl-R opens the finder", opened, t.text()[-300:])
    t.type(f"ok-{tag}"); time.sleep(1.2)
    t.keys("enter"); time.sleep(1.2)
    check("Enter puts the pick on the command line", f"echo ok-{tag}" in t.last_prompt(), t.last_prompt())
    t.keys("ctrl_u"); time.sleep(0.3)
    t.keys("up")
    ok = t.wait(r"in this folder ←→", 15)
    time.sleep(1.2)
    bar = [l for l in t.text().splitlines() if "←→" in l]
    n = re.search(r"(\d+) (?:of \d+|results?)", bar[-1]) if bar else None
    check("Up opens folder-scoped finder that sees PowerShell's history of this folder", ok and n is not None and int(n.group(1)) > 5, bar[-1] if bar else "")
    t.keys("esc"); time.sleep(0.8)
    t.keys("ctrl_u"); time.sleep(0.3)
    t.keys("\t")
    check("Tab on an empty line opens the finder", t.wait(r"in this folder ←→", 12), t.text()[-200:])
    t.keys("esc"); time.sleep(0.8)
    # Tab completion for reman itself (same engine as PowerShell: `reman complete`). Fresh prompt
    # first: bash doesn't redraw its prompt after the empty-line finder returns
    t.keys("ctrl_c"); time.sleep(0.8)
    t.type("reman con"); t.keys("\t"); time.sleep(2.0)
    check("Tab completes `reman con` -> `reman connect`", "reman connect" in t.last_prompt(), t.last_prompt())
    t.keys("ctrl_u"); time.sleep(0.3)
    t.type("reman init fi"); t.keys("\t"); time.sleep(2.0)
    check("Tab completes a value (`reman init fi` -> fish)", "reman init fish" in t.last_prompt(), t.last_prompt())
    t.keys("ctrl_u"); time.sleep(0.3)
    t.type("reman connect --old"); t.keys("\t"); time.sleep(2.0)
    check("Tab completes a flag (`--old` -> --old-history)", "--old-history" in t.last_prompt(), t.last_prompt())
    t.keys("ctrl_u"); time.sleep(0.3)
    # latency of the capture path itself, measured inside the shell
    if sh == "bash":
        t.run('__s=$EPOCHREALTIME; for i in {1..20}; do __reman_last_num=; __reman_start=1; __reman_precmd; done; echo "LAT $(( (${EPOCHREALTIME/[.,]/} - ${__s/[.,]/}) / 20 ))us"', 4)
    elif sh == "zsh":
        t.run('__s=$EPOCHREALTIME; for i in {1..20}; do __reman_cmd="echo lat-probe"; __reman_start=$EPOCHREALTIME; __reman_precmd; done; integer __us; (( __us = (EPOCHREALTIME - __s) * 1000000 / 20 )); echo "LAT ${__us}us"', 4)
    else:
        # the real success path: fish_postexec with exit 0 (spool append, builtins only)
        t.run('for i in (seq 20); __reman_postexec "echo lat-probe"; end', 6)
        t.run('echo LAT (math "round($CMD_DURATION * 1000 / 20)")us', 2)
    m = re.search(r"LAT (\d+)us", t.text())
    lat = int(m.group(1)) / 1000 if m else None
    check(f"per-command capture cost < 15 ms (measured {lat} ms)", lat is not None and lat < 15)
    summary[sh] = (sum(res), len(res), lat)
    t.run("exit", 0.5)

print("\n=========== summary ===========")
for sh, (ok, n, lat) in summary.items():
    print(f"  {sh:5}: {ok}/{n} checks   capture cost {lat} ms/command")
print("SHELLS RESULT:", "PASS" if all(ok == n for ok, n, _ in summary.values()) else "FAIL")
subprocess.run([EXE, "stop"], env=BASE_ENV, capture_output=True, timeout=30)
