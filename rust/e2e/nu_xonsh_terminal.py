"""nushell and xonsh with `reman init nu` / `reman init xonsh` loaded, in a real terminal (ConPTY,
read through a VT emulator), against a sandbox daemon (own db, port, spool and config):

  * a success is recorded through the spool (no process), a failure through reman-hook, with
    its exit code and folder;
  * after a typo, the fix is printed and Alt+F puts it on the line;
  * Up opens the finder for this folder and Enter puts the pick on the line; Ctrl+R opens it
    for all folders;
  * Alt+N on an empty line puts what usually runs next here on the line, with why;
  * `rcd <words>` goes to the folder where that ran.

Each shell runs when found: REMAN_NU / REMAN_XONSH (paths), else on PATH; a missing one is
skipped, not failed.

  python e2e/nu_xonsh_terminal.py
"""
import json, os, re, shutil, socket, subprocess, sys, tempfile, threading, time
import winpty, pyte

sys.stdout.reconfigure(encoding="utf-8")
HERE = os.path.dirname(os.path.abspath(__file__))
EXE = os.environ.get("REMAN_EXE_UNDER_TEST") or os.path.join(HERE, "..", "target", "release", "reman.exe")
PORT = 8790
ROWS, COLS = 34, 120
KEY = {"enter": "\r", "up": "\x1b[A", "esc": "\x1b", "ctrl_r": "\x12", "alt_f": "\x1bf", "alt_n": "\x1bn", "ctrl_u": "\x15", "ctrl_c": "\x03"}
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
            if "\x1b[6n" in data:
                with self.lock:
                    y, x = self.screen.cursor.y, self.screen.cursor.x
                self.p.write(f"\x1b[{y + 1};{x + 1}R")
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
            elif time.time() - since >= quiet and not any("searching" in l for l in now):
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

    def run(self, cmd, wait=1.5):
        self.type(cmd)
        self.keys("enter")
        time.sleep(wait)

    def line(self):
        """The text after the last prompt."""
        # (after the whole-screen finder, ConPTY may leave some of its last frame on the line)
        ls = [l for l in self.lines() if self.prompt in l or l.endswith(self.prompt.rstrip())]
        return ls[-1].rsplit(self.prompt.rstrip(), 1)[-1].lstrip(" ") if ls else ""

    def clear(self):
        with self.lock:
            self.screen.reset()

    def close(self):
        try:
            self.p.terminate(force=True)
        except Exception:
            pass


def show(lines):
    return "\n".join(f"    {i:2}|{l}" for i, l in enumerate(lines))


def find_shell(var, names):
    p = os.environ.get(var)
    if p and os.path.exists(p):
        return p
    for n in names:
        w = shutil.which(n)
        if w:
            return w
    return None


def recorded(cmd, timeout=8):
    end = time.time() + timeout
    while time.time() < end:
        r = call({"op": "runs", "command": cmd})
        if r.get("runs"):
            return r["runs"][0]
        time.sleep(0.4)
    return None


def exercise(name, t, proj, other):
    print(f"\n{name}:")
    check(f"{name}: prompt", t.wait("(?m)^" + re.escape(t.prompt.strip()), 40), t.text())
    time.sleep(1.0)
    tag = name.split()[0]
    t.run(f"echo zz-{tag}-ok")
    r = recorded(f"echo zz-{tag}-ok")
    check(f"{name}: a success is recorded, with its folder", r and r["exit"] == 0 and os.path.normcase(r["folder"] or "") == os.path.normcase(proj), r)
    code = 3 if tag == "nu" else 4
    t.run(f"cmd /c exit {code}", 3.0)
    r = recorded(f"cmd /c exit {code}")
    check(f"{name}: a failure is recorded with its exit code", r and r["exit"] == code, r)
    t.run("gti status", 4.0)
    check(f"{name}: after a typo, the fix is printed", t.wait(r"reman: .*(->|→) git status", 10), t.text())
    t.keys("alt_f")
    time.sleep(0.6)
    check(f"{name}: Alt+F puts the fix on the line", t.line().strip() == "git status", repr(t.line()) + "\n" + show(t.lines()))
    t.keys("ctrl_u") if tag != "xonsh" else t.keys("esc")
    t.keys("ctrl_c")
    time.sleep(0.8)
    t.clear()
    t.run("echo zz-redraw", 1.0)
    t.keys("up")
    check(f"{name}: Up opens the finder", t.wait(TABS, 15), t.text())
    t.settle()
    t.type("npm te")
    t.settle()
    t.keys("enter")
    time.sleep(1.2)
    t.settle()
    check(f"{name}: Enter puts the pick on the line", t.line().strip() == "npm test", repr(t.line()) + "\n" + show(t.lines()))
    t.keys("ctrl_c")
    time.sleep(0.8)
    t.keys("ctrl_r")
    check(f"{name}: Ctrl+R opens the finder (all folders)", t.wait(TABS, 15) and t.wait("everywhere", 10), t.text())
    t.keys("esc")
    time.sleep(1.0)
    t.keys("alt_n")
    time.sleep(2.0)
    got = t.settle()
    # (nushell repaints its prompt over anything a key binding prints: the line only, there)
    check(f"{name}: Alt+N on an empty line: what usually runs next here" + ("" if tag == "nu" else ", and why"),
          t.line().strip() in ("npm test", "git status", "rcd billing") and (tag == "nu" or "reman:" in "\n".join(got)), repr(t.line()) + "\n" + show(got))
    t.keys("ctrl_c")
    time.sleep(0.8)
    t.run("rcd billing", 3.0)
    t.run("echo $\"cwd=(pwd)\"" if tag == "nu" else "echo @('cwd=' + $PWD)", 1.5)
    own = [c for c in ("__reman_find folder", "__reman_find all", "__reman_insert_fix", "__reman_nextup") if call({"op": "runs", "command": c}).get("runs")]
    check(f"{name}: reman's own keys are never recorded as commands", not own, own)
    check(f"{name}: rcd goes to the folder where that ran", any(l.startswith("cwd=") and l.rstrip().endswith("billing-api") for l in t.lines()), show(t.lines()))


def main():
    shells = [("nu (nushell)", find_shell("REMAN_NU", ["nu"])), ("xonsh", find_shell("REMAN_XONSH", ["xonsh"]))]
    if not any(p for _, p in shells):
        print("neither nushell nor xonsh found (REMAN_NU / REMAN_XONSH, or on PATH): nothing to test")
        return 0
    tmp = tempfile.mkdtemp(prefix="reman-nuxsh-")
    proj = os.path.join(tmp, "shop")
    other = os.path.join(tmp, "billing-api")
    os.makedirs(proj)
    os.makedirs(other)
    env = dict(os.environ, REMAN_PORT=str(PORT), REMAN_DB=os.path.join(tmp, "reman.db"), REMAN_SPOOL=os.path.join(tmp, "spool.jsonl"),
               REMAN_CONFIG=os.path.join(tmp, "config.json"), REMAN_CONNECT_HOME=os.path.join(tmp, "home"), PROJ=proj)
    for k in ("CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT", "AGENT", "REMAN_SESSION"):
        env.pop(k, None)
    with open(env["REMAN_CONFIG"], "w") as f:
        json.dump({"finder_height": 0}, f)
    proc = subprocess.Popen([EXE, "daemon", "--port", str(PORT)], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(240):
            try:
                call({"op": "ping"}, timeout=2)
                break
            except OSError:
                time.sleep(0.5)
        base = int(time.time()) - 2 * 86400
        for k in range(3):
            for i, c in enumerate(["git status", "npm test"]):
                call({"op": "ingest", "command": c, "exit": 0, "cwd": proj, "session": f"seed{k}", "actor": "human", "ts": base + k * 3600 + i * 60})
        call({"op": "ingest", "command": "stripe listen --forward-to localhost:4242", "exit": 0, "cwd": other, "session": "seed-b", "actor": "human", "ts": base})
        for name, path in shells:
            if not path:
                print(f"\n{name}: not found, skipped")
                continue
            tag = name.split()[0]
            script = subprocess.run([EXE, "init", tag], env=env, capture_output=True, text=True, encoding="utf-8").stdout
            if tag == "nu":
                rc = os.path.join(tmp, "reman.nu")
                open(rc, "w", encoding="utf-8").write(script)
                cfg = os.path.join(tmp, "config.nu")
                open(cfg, "w", encoding="utf-8").write(
                    "$env.PROMPT_COMMAND = {|| 'NU' }\n$env.PROMPT_COMMAND_RIGHT = {|| '' }\n$env.PROMPT_INDICATOR = {|| '> ' }\n"
                    "$env.config.show_banner = false\n"
                    f"source '{rc}'\n")
                envf = os.path.join(tmp, "env.nu")
                open(envf, "w").write("")
                argv, tenv = [path, "--config", cfg, "--env-config", envf], env
                t = Term(argv, tenv, "NU> ")
            else:
                rc = os.path.join(tmp, "reman.xsh")
                open(rc, "w", encoding="utf-8").write(script)
                xrc = os.path.join(tmp, "xonshrc.xsh")
                open(xrc, "w", encoding="utf-8").write(
                    "$PROMPT = 'XO> '\n$RIGHT_PROMPT = ''\n$BOTTOM_TOOLBAR = ''\n$XONSH_PROMPT_AUTO_SUGGEST = False\n$UPDATE_COMPLETIONS_ON_KEYPRESS = False\n"
                    "$XONSH_SHOW_TRACEBACK = True\n$COMMANDS_CACHE_SAVE_INTERMEDIATE = False\n"
                    f"source {rc!r}\n")
                argv, tenv = [path, "-i"], dict(env, XONSHRC=xrc)
                t = Term(argv, tenv, "XO> ")
            try:
                exercise(name, t, proj, other)
            finally:
                t.close()
            # the same shell inside `reman shell`: what a command prints is kept
            t = Term([EXE, "shell", "--", *argv], tenv, t.prompt)
            try:
                t.wait("(?m)^" + re.escape(t.prompt.strip()), 40)
                time.sleep(1.5)
                cmd = f"echo zz-{tag}-inside"
                t.run(cmd, 3.0)
                end, out = time.time() + 10, None
                while time.time() < end and not out:
                    runs = call({"op": "runs", "command": cmd}).get("runs") or []
                    out = runs[0].get("output") if runs else None
                    time.sleep(0.4)
                check(f"{name}: inside reman shell, what a command prints is kept", out == f"zz-{tag}-inside", repr(out) + "\n" + json.dumps(call({"op": "runs", "command": cmd}))[:600] + "\n" + json.dumps(call({"op": "outputs", "k": 3}))[:600] + "\n" + t.text())
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
    print(f"\nNU+XONSH RESULT: {'PASS' if passed == len(results) else 'FAIL'} ({passed}/{len(results)})")
    return 0 if passed == len(results) else 1


if __name__ == "__main__":
    sys.exit(main())
