"""reman's keys as the user set them (`keys` in config.json), in real terminals (ConPTY, read
through a VT emulator), against a sandbox daemon and config: PowerShell, bash, zsh and fish.

With ↑ and Tab given back, the finder on Alt+J and the fix on Alt+K:
  * ↑ is the shell's own again (the last command comes back), and Alt+J opens the finder;
  * Alt+K inserts the fix after a typo;
  * in the finder (PowerShell), Ctrl+E is "every run" instead of Ctrl+O, and the hint line says so;
  * a change made while the shell is open applies at its next prompt: Down for the finder, the
    fix on Alt+M (its message too), and keys reman no longer uses do what they did before.

  python e2e/keys_terminal.py
"""
import json, os, re, shutil, socket, subprocess, sys, tempfile, threading, time
import winpty, pyte

sys.stdout.reconfigure(encoding="utf-8")
HERE = os.path.dirname(os.path.abspath(__file__))
EXE = os.environ.get("REMAN_EXE_UNDER_TEST") or os.path.join(HERE, "..", "target", "release", "reman.exe")
EXE_FWD = EXE.replace("\\", "/")
PORT = 8783
ROWS, COLS = 30, 120
TABS = "Recall  Fixes  Flows"
KEY = {"enter": "\r", "up": "\x1b[A", "down": "\x1b[B", "alt_m": "\x1bm", "esc": "\x1b", "alt_j": "\x1bj", "alt_k": "\x1bk", "alt_l": "\x1bl", "ctrl_e": "\x05", "ctrl_o": "\x0f", "ctrl_u": "\x15", "ctrl_c": "\x03"}
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
    def __init__(self, argv, env, prompt):
        self.prompt = prompt
        self.screen = pyte.Screen(COLS, ROWS)
        self.stream = pyte.Stream(self.screen)
        self.lock = threading.Lock()
        self.p = winpty.PtyProcess.spawn(argv, env=env, dimensions=(ROWS, COLS), cwd=env["PROJ"])
        threading.Thread(target=self._pump, daemon=True).start()

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
                if "\x1b[6n" in data:
                    self.p.write(f"\x1b[{self.screen.cursor.y + 1};{self.screen.cursor.x + 1}R")

    def text(self):
        with self.lock:
            return "\n".join(l.rstrip() for l in self.screen.display)

    def wait(self, pattern, timeout=20):
        end = time.time() + timeout
        while time.time() < end:
            if re.search(pattern, self.text()):
                return True
            time.sleep(0.1)
        return False

    def keys(self, *ks, wait=0.4):
        for k in ks:
            self.p.write(KEY.get(k, k))
            time.sleep(wait)

    def type(self, s):
        for ch in s:
            self.p.write(ch)
            time.sleep(0.03)

    def run(self, cmd, wait=1.5):
        self.type(cmd)
        self.keys("enter")
        time.sleep(wait)

    def clear(self):
        with self.lock:
            self.screen.reset()

    def line(self):
        ls = [l for l in self.text().splitlines() if self.prompt in l]
        return ls[-1].rsplit(self.prompt, 1)[-1] if ls else ""

    def close(self):
        try:
            self.p.terminate(force=True)
        except Exception:
            pass


def start(sh, env, tmp):
    d = tempfile.mkdtemp(prefix=f"keys-{sh}-", dir=tmp)
    env = dict(env)
    if sh == "powershell":
        prof = os.path.join(d, "profile.ps1")
        # a prompt of the user's own, set before reman (which builds on it), as a profile does
        open(prof, "w", encoding="utf-8-sig").write(f"Import-Module PSReadLine\nfunction global:prompt {{ 'PS> ' }}\n& '{EXE}' init powershell | Out-String | Invoke-Expression\n")
        t = Term("powershell.exe -NoLogo -NoProfile", env, "PS> ")
        t.wait(r"PS [A-Z]:\\", 40)
        time.sleep(0.8)
        t.run(f". '{prof}'", 4.0)
        return t
    if sh == "bash":
        rc = os.path.join(d, "bashrc")
        open(rc, "w", newline="\n").write(f'PS1="RMN> "\neval "$("{EXE_FWD}" init bash)"\n')
        return Term([r"C:\Program Files\Git\usr\bin\bash.exe", "--noprofile", "--rcfile", rc.replace("\\", "/"), "-i"], env, "RMN> ")
    if sh == "zsh":
        open(os.path.join(d, ".zshrc"), "w", newline="\n").write(f'PROMPT="RMN> "\neval "$("{EXE_FWD}" init zsh)"\n')
        env.update(ZDOTDIR=d.replace("\\", "/"), MSYSTEM="MSYS", PATH=r"D:\msys64\usr\bin;" + env["PATH"])
        return Term([r"D:\msys64\usr\bin\zsh.exe", "-i"], env, "RMN> ")
    os.makedirs(os.path.join(d, "fish"))
    open(os.path.join(d, "fish", "config.fish"), "w", newline="\n").write(
        f'function fish_prompt; echo -n "RMN> "; end\nfunction fish_greeting; end\n"{EXE_FWD}" init fish | source\n')
    env.update(XDG_CONFIG_HOME=d.replace("\\", "/"), MSYSTEM="MSYS", PATH=r"D:\msys64\usr\bin;" + env["PATH"])
    return Term([r"D:\msys64\usr\bin\fish.exe", "-i"], env, "RMN> ")


def exercise(sh, t, cfg):
    print(f"\n{sh}:")
    check(f"{sh}: starts", t.wait(re.escape(t.prompt.strip()), 40), t.text())
    time.sleep(1.0)
    errors = ("FullyQualifiedErrorId", "Exception", "command not found", "Unknown command", "parse error", "bad pattern", "no such", "Error:")
    check(f"{sh}: no error while reman loads", not any(e in t.text() for e in errors), t.text())
    t.clear()
    t.run(f"echo zz-up-{sh}", 1.5)
    t.keys("up", wait=1.5)
    check(f"{sh}: Up is the shell's own again (the last command comes back, no finder)", t.line().strip() == f"echo zz-up-{sh}" and TABS not in t.text(), repr(t.line()) + "\n" + t.text())
    t.keys("ctrl_u" if sh != "powershell" else "esc")
    t.keys("alt_j", wait=2.5)
    check(f"{sh}: Alt+J opens the finder", t.wait(TABS, 15), t.text())
    if sh == "powershell":
        t.type("git status")
        time.sleep(2.0)
        check(f"{sh}: the finder's hint line names Ctrl+E for every run", "^E every run" in t.text() and "^O every run" not in t.text(), t.text())
        t.keys("ctrl_o", wait=1.5)
        check(f"{sh}: Ctrl+O no longer opens every run", "every run of" not in t.text(), t.text())
        t.keys("ctrl_e", wait=2.5)
        check(f"{sh}: Ctrl+E does", t.wait("every run of", 10), t.text())
        t.keys("esc", wait=0.8)
    t.keys("esc", wait=1.5)
    t.clear()
    t.run("gti status", 3.0)
    t.keys("alt_k", wait=1.0)
    check(f"{sh}: Alt+K inserts the fix", t.line().strip() == "git status", repr(t.line()) + "\n" + t.text())
    t.keys("ctrl_u" if sh != "powershell" else "esc")
    # changed while the shell is open: its next prompt takes it up. The finder here moves to Down,
    # every folder to Alt+L, the fix to Alt+M; Alt+J goes back to what it did before
    time.sleep(1.2)
    with open(cfg, "w") as f:
        json.dump({"finder_height": 0, "keys": {"find_here": ["Down"], "tab": [], "find_all": ["Alt+L"], "fix": ["Alt+M"]}}, f)
    time.sleep(1.2)
    t.run(f"echo zz-reload-{sh}", 2.0)
    t.clear()
    t.keys("alt_l", wait=2.5)
    check(f"{sh}: a change applies at the next prompt (Alt+L opens the finder)", t.wait(TABS, 15), t.text())
    t.keys("esc", wait=1.5)
    t.clear()
    t.keys("alt_j", wait=2.0)
    check(f"{sh}: ...and Alt+J is given back (no finder)", TABS not in t.text(), t.text())
    t.keys("ctrl_u" if sh != "powershell" else "esc")
    t.clear()
    t.keys("down", wait=2.5)
    check(f"{sh}: Down opens the finder for this folder", t.wait(TABS, 15), t.text())
    t.keys("esc", wait=1.5)
    t.clear()
    t.keys("enter", wait=1.0)
    t.keys("up", wait=1.5)
    # (the screen was cleared first: the command showing at all means Up brought it back)
    check(f"{sh}: Up is still the shell's own", f"echo zz-reload-{sh}" in t.text() and TABS not in t.text(), repr(t.line()) + "\n" + t.text())
    t.keys("ctrl_u" if sh != "powershell" else "esc")
    t.clear()
    t.run("gti status", 3.0)
    check(f"{sh}: the fix line names the new key", "Alt+M inserts" in t.text(), t.text())
    t.keys("alt_m", wait=1.0)
    check(f"{sh}: Alt+M inserts the fix", t.line().strip() == "git status", repr(t.line()) + "\n" + t.text())
    t.keys("ctrl_u" if sh != "powershell" else "esc")
    # back to the standard keys: Up is reman's again, Down the shell's
    time.sleep(1.2)
    with open(cfg, "w") as f:
        json.dump({"finder_height": 0}, f)
    time.sleep(1.2)
    t.run(f"echo zz-std-{sh}", 2.0)
    t.clear()
    t.keys("up", wait=2.5)
    check(f"{sh}: back to standard, Up opens the finder again", t.wait(TABS, 15), t.text())
    t.keys("esc", wait=1.5)
    t.clear()
    t.keys("down", wait=2.0)
    check(f"{sh}: ...and Down is given back (no finder)", TABS not in t.text(), t.text())
    t.keys("ctrl_u" if sh != "powershell" else "esc")

def main():
    tmp = tempfile.mkdtemp(prefix="reman-keys-")
    proj = os.path.join(tmp, "shop")
    os.makedirs(proj)
    cfg = os.path.join(tmp, "config.json")
    env = dict(os.environ, REMAN_PORT=str(PORT), REMAN_DB=os.path.join(tmp, "reman.db"), REMAN_SPOOL=os.path.join(tmp, "spool.jsonl"),
               REMAN_CONFIG=cfg, REMAN_CONNECT_HOME=os.path.join(tmp, "home"), PROJ=proj, TERM="xterm-256color")
    for k in ("CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT", "AGENT", "REMAN_SESSION", "REMAN_PTY"):
        env.pop(k, None)
    proc = subprocess.Popen([EXE, "daemon", "--port", str(PORT)], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(240):
            try:
                call({"op": "ping"}, timeout=2)
                break
            except OSError:
                time.sleep(0.5)
        base = int(time.time()) - 86400
        for k in range(3):
            call({"op": "ingest", "command": "git status", "exit": 0, "cwd": proj, "session": f"seed{k}", "actor": "human", "ts": base + k * 60})
        for sh in ("powershell", "bash", "zsh", "fish"):
            with open(cfg, "w") as f:
                json.dump({"finder_height": 0, "keys": {"find_here": [], "tab": [], "find_all": ["Alt+J"], "fix": ["Alt+K"], "runs": ["Ctrl+E"]}}, f)
            t = start(sh, env, tmp)
            try:
                exercise(sh, t, cfg)
            finally:
                t.close()
    finally:
        try:
            call({"op": "shutdown"}, timeout=5)
        except OSError:
            pass
        proc.kill()
        time.sleep(0.5)
        shutil.rmtree(tmp, ignore_errors=True)
    passed = sum(results)
    print(f"\nKEYS RESULT: {'PASS' if passed == len(results) else 'FAIL'} ({passed}/{len(results)})")
    return 0 if passed == len(results) else 1


if __name__ == "__main__":
    sys.exit(main())
