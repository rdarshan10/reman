"""What commands print, kept: `reman shell` (reman's own terminal layer, ConPTY here) running
PowerShell with `reman init powershell`, in a real terminal read through a VT emulator, against a
sandbox daemon (own db, port, spool and config):

  * the shell inside knows it is (REMAN_PTY), and the screen passes through untouched;
  * a command's output is kept with its run: text only, no prompt or echo, colours gone, a
    progress line as it ended, long output whole, secrets masked;
  * a native program's output and exit code; `reman output` finds it by command or by what it
    printed; the finder's Ctrl+O shows a run's output;
  * a resize reaches the shell inside; `exit 5` leaves with 5;
  * agents' output (from their hooks) is kept too, and only the newest `output_runs` runs keep it.

  python e2e/output_capture.py
"""
import json, os, re, shutil, socket, subprocess, sys, tempfile, threading, time
import winpty, pyte

sys.stdout.reconfigure(encoding="utf-8")
HERE = os.path.dirname(os.path.abspath(__file__))
EXE = os.environ.get("REMAN_EXE_UNDER_TEST") or os.path.join(HERE, "..", "target", "release", "reman.exe")
PORT = 8789
ROWS, COLS = 30, 120
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
    def __init__(self, argv, env, cwd, answers=True):
        self.answers, self.asked = answers, 0
        self.screen = pyte.Screen(COLS, ROWS)
        self.stream = pyte.Stream(self.screen)
        self.lock = threading.Lock()
        self.raw = ""
        self.env_used, self.cwd_used = env, cwd
        self.p = winpty.PtyProcess.spawn(argv, env=env, dimensions=(ROWS, COLS), cwd=cwd)
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
                self.raw += data
                self.stream.feed(data)
                # real terminals answer where the cursor is; `answers=False` is one that doesn't
                if "\x1b[6n" in data:
                    self.asked += 1
                if "\x1b[6n" in data and self.answers:
                    self.p.write(f"\x1b[{self.screen.cursor.y + 1};{self.screen.cursor.x + 1}R")

    def text(self):
        with self.lock:
            return "\n".join(l.rstrip() for l in self.screen.display)

    def lines(self):
        with self.lock:
            return [l.rstrip() for l in self.screen.display]

    def wait(self, pattern, timeout=20):
        end = time.time() + timeout
        while time.time() < end:
            if re.search(pattern, self.text()):
                return True
            time.sleep(0.1)
        return False

    def keys(self, s, wait=0.3):
        self.p.write(s)
        time.sleep(wait)

    def type(self, s):
        for ch in s:
            self.p.write(ch)
            time.sleep(0.02)

    def run(self, cmd, wait=1.5):
        self.type(cmd)
        self.keys("\r")
        time.sleep(wait)


def show(lines):
    return "\n".join(f"    {i:2}|{l}" for i, l in enumerate(lines))


def kept(cmd, timeout=10):
    """The newest run of `cmd` once its output is kept."""
    end = time.time() + timeout
    while time.time() < end:
        runs = call({"op": "runs", "command": cmd}).get("runs") or []
        if runs and runs[0].get("output") is not None:
            return runs[0]
        time.sleep(0.4)
    runs = call({"op": "runs", "command": cmd}).get("runs") or []
    return runs[0] if runs else None


def main():
    tmp = tempfile.mkdtemp(prefix="reman-capture-")
    proj = os.path.join(tmp, "shop")
    os.makedirs(proj)
    cfg = os.path.join(tmp, "config.json")
    with open(cfg, "w") as f:
        json.dump({"finder_height": 0}, f)
    env = dict(os.environ, REMAN_PORT=str(PORT), REMAN_DB=os.path.join(tmp, "reman.db"), REMAN_SPOOL=os.path.join(tmp, "spool.jsonl"),
               REMAN_CONFIG=cfg, REMAN_CONNECT_HOME=os.path.join(tmp, "home"))
    for k in ("CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT", "AGENT", "REMAN_SESSION", "REMAN_PTY", "WT_SESSION"):
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
        prof = os.path.join(tmp, "profile.ps1")
        open(prof, "w", encoding="utf-8-sig").write(f"Import-Module PSReadLine\n& '{EXE}' init powershell | Out-String | Invoke-Expression\nClear-Host\n")
        t = Term([EXE, "shell", "--", "powershell.exe", "-NoLogo", "-NoProfile"], env, proj)
        run(t, prof, cfg, tmp)
    finally:
        if t:
            try:
                t.p.terminate(force=True)
            except Exception:
                pass
        try:
            call({"op": "shutdown"}, timeout=5)
        except OSError:
            pass
        proc.kill()
        time.sleep(0.5)
        shutil.rmtree(tmp, ignore_errors=True)
    passed = sum(results)
    print(f"\nCAPTURE RESULT: {'PASS' if passed == len(results) else 'FAIL'} ({passed}/{len(results)})")
    return 0 if passed == len(results) else 1


def run(t, prof, cfg, tmp):
    print("reman shell, PowerShell inside:")
    check("a prompt comes up through it", t.wait(r"PS [A-Z]:\\", 40), t.text())
    time.sleep(1.0)
    t.run(f". '{prof}'", 4.0)
    t.run("echo \"pty=$env:REMAN_PTY\"")
    check("the shell inside knows (REMAN_PTY)", "pty=1" in t.text(), t.text())
    check("no marks reach the screen", "46031711" not in t.text() and "\x1b]46031711" not in t.raw[-20000:], t.raw[-400:])

    print("\nwhat a command prints, kept:")
    c1 = 'Write-Output "zz-cap-1"; Write-Output "second line"'
    t.run(c1, 2.0)
    r = kept(c1)
    out = (r or {}).get("output") or ""
    check("kept with its run", out.splitlines() == ["zz-cap-1", "second line"], repr(out) + " " + json.dumps(r)[:300] + "\n" + json.dumps(call({"op": "outputs", "k": 10}))[:1500])
    c2 = 'Write-Host -NoNewline "10%"; Write-Host -NoNewline "`r100%"; Write-Host " done" -ForegroundColor Green'
    t.run(c2, 2.0)
    out = (kept(c2) or {}).get("output") or ""
    check("a progress line as it ended, colours gone", out == "100% done", repr(out))
    c3 = '1..300 | % { "row $_" }'
    t.run(c3, 3.0)
    out = (kept(c3) or {}).get("output") or ""
    ls = out.splitlines()
    check("long output whole, what scrolled off too", ls[:1] == ["row 1"] and ls[-1:] == ["row 300"] and len(ls) == 300, f"{len(ls)} lines: {ls[:2]} ... {ls[-2:]}")
    c4 = 'Write-Output "export API_TOKEN=abc123secret"'
    t.run(c4, 2.0)
    time.sleep(2.0)
    # (the command's own text is kept masked too, so it's found by what it printed)
    m = call({"op": "outputs", "query": "API_TOKEN", "k": 3}).get("results", [])
    out = m[0]["output"] if m else ""
    check("secrets masked, in the output and the command", out == "export API_TOKEN=***" and "abc123secret" not in m[0]["command"], m)
    c5 = 'cmd /c "echo native-out & exit 3"'
    t.run(c5, 2.5)
    r = kept(c5) or {}
    check("a native program: its output and exit code", (r.get("output"), r.get("exit")) == ("native-out", 3), r)

    print("\nreman output:")
    t.run("Clear-Host", 1.0)
    t.run("& $global:__RemanExe output native", 4.0)
    check("finds a run by what it printed (or its command)", t.wait(r"│ native-out", 10), t.text())
    t.run("Clear-Host", 1.0)
    t.run("& $global:__RemanExe output --failed", 4.0)
    check("--failed: the failed run only", "native-out" in t.text() and "zz-cap-1" not in t.text(), t.text())

    print("\nthe finder's Ctrl+O shows it:")
    t.run("Clear-Host", 1.0)
    t.keys("\x1b[A", 0.5)
    t.wait(TABS, 15)
    time.sleep(1.0)
    t.type("second line")
    time.sleep(2.0)
    t.keys("\x0f", 2.5)
    check("the selected run's output, beside its runs", t.wait("what it printed", 10) and "zz-cap-1" in t.text(), t.text())
    t.keys("\x1b", 0.6)
    t.keys("\x1b", 1.0)

    print("\nresize and exit:")
    t.p.setwinsize(ROWS, 100)
    with t.lock:
        t.screen.resize(ROWS, 100)
    time.sleep(1.5)
    t.run("Clear-Host", 1.0)
    t.run("echo \"w=$($Host.UI.RawUI.WindowSize.Width)\"", 1.5)
    check("a resize reaches the shell inside", "w=100" in t.text(), t.text())
    t.run("exit 5", 3.0)
    end = time.time() + 10
    while t.p.isalive() and time.time() < end:
        time.sleep(0.2)
    check("exit 5 leaves reman shell with 5", not t.p.isalive() and t.p.exitstatus == 5, f"alive={t.p.isalive()} status={t.p.exitstatus}")

    print("\na terminal that never says where its cursor is:")
    t4 = Term([EXE, "shell", "--", "cmd.exe", "/k", "prompt ZZ$G"], t.env_used, t.cwd_used, answers=False)
    try:
        up = t4.wait("ZZ>", 15)
        t4.run("echo zz-silent-ok", 1.5)
        check("the shell still starts (reman answers from the console itself)", up and "zz-silent-ok" in t4.text().split("echo zz-silent-ok", 1)[-1], t4.text())
        check("...and never asks the terminal, so no reply can land in the shell", t4.asked == 0 and ";1R" not in t4.text(), t4.asked)
    finally:
        t4.p.terminate(force=True)

    print("\nshells opening inside reman shell (the setting):")
    with open(cfg, "w") as f:
        json.dump({"finder_height": 0, "shell_layer": True}, f)
    env = dict(t.env_used)
    t2 = Term(["powershell.exe", "-NoLogo", "-NoProfile"], env, t.cwd_used)
    try:
        t2.wait(r"PS [A-Z]:\\", 40)
        time.sleep(1.0)
        t2.run(f". '{prof}'", 6.0)
        t2.run("echo \"pty=$env:REMAN_PTY\"", 2.0)
        check("on: a new shell hands over to one inside reman shell", "pty=1" in t2.text(), t2.text())
        t2.run("exit 7", 3.0)
        end = time.time() + 10
        while t2.p.isalive() and time.time() < end:
            time.sleep(0.2)
        check("...and closes with it, with its exit code", not t2.p.isalive() and t2.p.exitstatus == 7, f"alive={t2.p.isalive()} status={t2.p.exitstatus}")
    finally:
        if t2.p.isalive():
            t2.p.terminate(force=True)
    with open(cfg, "w") as f:
        json.dump({"finder_height": 0}, f)
    t3 = Term(["powershell.exe", "-NoLogo", "-NoProfile"], env, t.cwd_used)
    try:
        t3.wait(r"PS [A-Z]:\\", 40)
        time.sleep(1.0)
        t3.run(f". '{prof}'", 5.0)
        t3.run("echo \"pty=[$env:REMAN_PTY]\"", 2.0)
        check("off (the default): shells open as before", "pty=[]" in t3.text(), t3.text())
    finally:
        t3.p.terminate(force=True)

    print("\nagents' output, and how much is kept:")
    printed = "Running 3 tests\n\x1b[31mFAIL\x1b[0m src/a.test.ts\nexpected 2, got 3\nGITHUB_TOKEN=ghp_aaaaaaaaaaaaaaaaaaaaaaaaaaaa\n"
    call({"op": "ingest", "command": "npx vitest run", "exit": 1, "cwd": tmp, "session": "ag1", "actor": "agent:claude-code", "printed": printed})
    r = kept("npx vitest run") or {}
    check("an agent's output is kept, cleaned and masked", r.get("output") == "Running 3 tests\nFAIL src/a.test.ts\nexpected 2, got 3\nGITHUB_TOKEN=***", repr(r.get("output")))
    m = call({"op": "outputs", "query": "expected 2, got 3"})
    check("found by what it printed", [x["command"] for x in m.get("results", [])][:1] == ["npx vitest run"], m)
    m = call({"op": "mcp", "tool": "reman_output", "args": {"command": "npx vitest run"}, "roots": [tmp]})
    got = (m.get("runs") or [{}])[0]
    check("agents read it over MCP (reman_output), masked", got.get("exit") == 1 and "expected 2, got 3" in got.get("output", "") and "ghp_" not in json.dumps(m), m)
    m = call({"op": "mcp", "tool": "reman_output", "args": {"command": "npx vitest run"}, "roots": [r"C:\somewhere\else"]})
    check("...never outside the folders shared with them", m.get("runs") == [] and m.get("note"), m)
    with open(cfg, "w") as f:
        json.dump({"finder_height": 0, "output_runs": 0}, f)
    time.sleep(1.1)
    call({"op": "ingest", "command": "npm run lint", "exit": 0, "cwd": tmp, "session": "ag1", "actor": "agent:claude-code", "printed": "all good"})
    r = (call({"op": "runs", "command": "npm run lint"}).get("runs") or [{}])[0]
    check("output_runs 0: nothing kept", r.get("output") is None, r)
    with open(cfg, "w") as f:
        json.dump({"finder_height": 0, "output_runs": 5}, f)
    time.sleep(1.1)
    for k in range(60):
        call({"op": "ingest", "command": f"echo bulk {k}", "exit": 0, "cwd": tmp, "session": "ag2", "actor": "agent:codex", "printed": f"bulk {k}"})
    n = len(call({"op": "outputs", "k": 50}).get("results", []))
    old = call({"op": "outputs", "query": "zz-cap-1"}).get("results", [])
    check("only the newest output_runs runs keep it (5; trimmed every 50 stores, so at most 54)",
          n <= 54 and not old and call({"op": "outputs", "k": 1})["results"][0]["output"] == "bulk 59", (n, old))


if __name__ == "__main__":
    sys.exit(main())
