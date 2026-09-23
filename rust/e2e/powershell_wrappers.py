"""Plain Windows PowerShell with the real profile. Prompt wrappers installed AFTER reman (a Python
venv's Activate.ps1, VS Code's shell integration) must not turn failures into successes, and the
wrapped prompt must still see the real $?."""
import os, sys, time, shutil, threading, json, socket, subprocess
import winpty, pyte

sys.stdout.reconfigure(encoding="utf-8")
TMP = os.environ["TEMP"]
DB = os.path.join(TMP, "reman-wrap.db")
EXE = os.path.join(os.path.expanduser("~"), ".reman", "bin", "reman.exe")
import glob
# VS Code's PowerShell shell integration (whichever VS Code build is installed) and a Python venv
_si = glob.glob(os.path.join(os.environ.get("LOCALAPPDATA", ""), "Programs", "Microsoft VS Code", "*", "resources", "app", "out",
                             "vs", "workbench", "contrib", "terminal", "common", "scripts", "shellIntegration.ps1"))
SI = _si[0] if _si else None
VENV = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", ".venv", "Scripts", "Activate.ps1")
if not os.path.exists(VENV):
    sys.exit("needs a Python venv at <repo>/.venv (python -m venv .venv)")
ENV = dict(os.environ, REMAN_PORT="8767", REMAN_DB=DB, REMAN_SPOOL=os.path.join(TMP, "reman-wrap-spool.jsonl"), VSCODE_NONCE="n", VSCODE_STABLE="1")
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

scr = pyte.Screen(130, 30)
st = pyte.Stream(scr)
lock = threading.Lock()
raw = []
p = winpty.PtyProcess.spawn("powershell.exe -NoLogo", env=ENV, dimensions=(30, 130), cwd=os.path.expanduser("~"))


def pump():
    while True:
        try:
            d = p.read(65536)
        except EOFError:
            return
        if "\x1b[c" in d:
            p.write("\x1b[?61;4;6;7;14;21;22;23;24;28;32;42c")
        raw.append(d)
        with lock:
            st.feed(d)


threading.Thread(target=pump, daemon=True).start()


def detail(c):
    s = socket.create_connection(("127.0.0.1", 8767), timeout=10)
    s.sendall((json.dumps({"op": "detail", "command": c}) + "\n").encode())
    d = b""
    while not d.endswith(b"\n"):
        d += s.recv(1 << 20)
    v = json.loads(d)
    return v.get("status"), v.get("runs")


def say(s):
    for ch in s:
        p.write(ch)
        time.sleep(0.03)
    p.write("\r")
    time.sleep(2)


results = []


def expect(c, want, tag):
    say(c)
    got = detail(c)[0]
    results.append(got == want)
    print(f"  {'ok  ' if got == want else 'FAIL'} [{tag}] {c:<30} recorded {got!s:<8} want {want}")


time.sleep(6)
expect("gti status-plain", "fail", "plain")
say(f"& '{VENV}'")                      # a venv wraps the prompt after reman
expect(r".\NoSuchDir-zz" + "\\", "fail", "venv")
expect("gti status-venv", "fail", "venv")
expect("cmd /c exit 4", "fail", "venv")
expect("echo venv-ok", "ok", "venv")
with lock:
    venv_prefix = any(l.startswith("(.venv) PS") or "(.venv)" in l for l in scr.display)
results.append(venv_prefix)
print(f"  {'ok  ' if venv_prefix else 'FAIL'} [venv] the venv's own prompt prefix still shows")
for ch in "reman con":
    p.write(ch); time.sleep(0.03)
p.write("\t"); time.sleep(2.5)
with lock:
    line = [l for l in scr.display if "PS " in l][-1]
tab_ok = line.rstrip().endswith("reman connect")
results.append(tab_ok)
print(f"  {'ok  ' if tab_ok else 'FAIL'} [venv] Tab still completes reman (`reman con` -> `reman connect`)")
p.write("\x1b"); time.sleep(0.3)
say("deactivate")
expect("gti status-after-deactivate", "fail", "deactivated")
if SI:
    say(f'try {{ . "{SI}" }} catch {{}}')     # VS Code's integration wraps it too
    raw.clear()
    expect("gti status-vscode", "fail", "vscode")
    vs_code = "".join(raw)
    fwd = "\x1b]633;D;1" in vs_code
    results.append(fwd)
    print(f"  {'ok  ' if fwd else 'FAIL'} [vscode] VS Code's own prompt still sees the failure (OSC 633;D;1)")
    expect("echo vscode-ok", "ok", "vscode")
else:
    print("  skip [vscode] VS Code not installed")
# an update to reman reloads itself into this open shell at the next prompt
# (the daemon holds reman.exe open, so pretend this shell loaded an older build instead)
say("$global:__RemanExeTime = [datetime]'2020-01-01'")
say("echo after-update")
with lock:
    reloaded = any("this shell now runs the new version" in l for l in scr.display)
results.append(reloaded)
print(f"  {'ok  ' if reloaded else 'FAIL'} [update] open shell reloads the integration after reman is updated")
expect("gti status-after-reload", "fail", "update")
expect("echo reload-ok", "ok", "update")
with lock:
    print("\n".join(l.rstrip() for l in scr.display if l.strip())[-500:])
p.write("exit\r")
time.sleep(0.5)
subprocess.run([EXE, "stop"], env=ENV, capture_output=True, timeout=30)
print("WRAP RESULT:", "PASS" if all(results) else "FAIL")
