"""`reman uninstall`, end to end in a sandbox: a fake ~/.reman (REMAN_HOME) with reman running
from its own bin folder, coding tools connected in a sandboxed home (REMAN_CONNECT_HOME), a
PowerShell profile and a .bashrc holding the user's own lines too (REMAN_PS_PROFILES). The
registry and PATH are never touched in a sandbox. Your real setup is never touched.

  * without --yes, and not in a terminal: nothing is removed;
  * --dry-run lists everything and changes nothing;
  * --yes removes reman from Claude Code and Codex (MCP + hooks), the HTTP endpoint, the
    profile's block and the rc line (keeping the user's lines), and deletes the program folder
    once it has exited, keeping the history;
  * --purge deletes the history too.

  python e2e/uninstall_sandbox.py
"""
import json, os, shutil, subprocess, sys, tempfile, time

HERE = os.path.dirname(os.path.abspath(__file__))
BUILD = os.path.join(HERE, "..", "target", "release")
EXE_NAME = "reman.exe" if os.name == "nt" else "reman"
results = []


def check(name, ok, detail=""):
    ok = bool(ok)
    results.append(ok)
    print(f"  {'ok  ' if ok else 'FAIL'} {name}" + (f"  ({str(detail)[:400]})" if detail and not ok else ""))


def read(p):
    return open(p, encoding="utf-8").read() if os.path.exists(p) else ""


def main():
    tmp = tempfile.mkdtemp(prefix="reman-uninstall-")
    try:
        run(tmp)
    finally:
        time.sleep(1)
        shutil.rmtree(tmp, ignore_errors=True)
    passed = sum(results)
    print(f"\nUNINSTALL RESULT: {'PASS' if passed == len(results) else 'FAIL'} ({passed}/{len(results)})")
    return 0 if passed == len(results) else 1


def run(tmp):
    home = os.path.join(tmp, ".reman")
    bin_dir = os.path.join(home, "bin")
    user = os.path.join(tmp, "home")
    os.makedirs(bin_dir)
    os.makedirs(os.path.join(user, ".codex"))
    for f in os.listdir(BUILD):
        if f.startswith(("reman.exe", "reman-hook.exe")) if os.name == "nt" else f in ("reman", "reman-hook"):
            shutil.copy(os.path.join(BUILD, f), bin_dir)
    exe = os.path.join(bin_dir, EXE_NAME)
    open(os.path.join(home, "reman.db"), "wb").write(b"history")
    os.makedirs(os.path.join(home, "models"))
    open(os.path.join(home, "models", "model.onnx"), "wb").write(b"x" * 1000)
    profile = os.path.join(tmp, "profile.ps1")
    open(profile, "w").write("Set-Alias ll ls\n\n# >>> reman >>>\n# Reman shell integration (managed by `reman setup`)\nif (Test-Path \"x\") { & \"x\" init powershell | Out-String | Invoke-Expression }\n# <<< reman <<<\n")
    bashrc = os.path.join(user, ".bashrc")
    open(bashrc, "w").write("export EDITOR=vim\n\neval \"$(\"/x/reman\" init bash)\"  # reman shell integration\n")
    cfg = os.path.join(home, "config.json")
    json.dump({"strict_secrets": True, "http": {"port": 8779, "token": "t"}}, open(cfg, "w"))
    env = dict(os.environ, REMAN_HOME=home, REMAN_CONNECT_HOME=user, REMAN_CONFIG=cfg, REMAN_PS_PROFILES=profile, REMAN_PORT="8791",
               REMAN_DB=os.path.join(home, "reman.db"))

    def reman(*args, exe=exe):
        p = subprocess.run([exe, *args], env=env, capture_output=True, text=True, encoding="utf-8", stdin=subprocess.DEVNULL)
        return p.returncode, p.stdout + p.stderr

    for tool in ("claude-code", "codex"):
        code, out = reman("connect", tool)
        check(f"setup for the test: connect {tool}", code == 0, out)
    claude = os.path.join(user, ".claude", "settings.json")
    codex_hooks = os.path.join(user, ".codex", "hooks.json")
    codex_cfg = os.path.join(user, ".codex", "config.toml")
    check("setup for the test: hooks and MCP entries are there", "reman" in read(claude) and "reman" in read(codex_hooks) and "reman" in read(codex_cfg))

    code, out = reman("uninstall")
    check("not in a terminal and no --yes: nothing is removed", code != 0 and "--yes" in out and os.path.exists(exe) and "reman" in read(claude), out)

    code, out = reman("uninstall", "--dry-run")
    print(out)
    for what in ("Claude Code", "OpenAI Codex CLI", "HTTP endpoint", profile, bashrc, bin_dir):
        check(f"--dry-run lists {what if len(what) < 40 else os.path.basename(what)}", what in out, out)
    check("--dry-run changes nothing", os.path.exists(exe) and ">>> reman >>>" in read(profile) and "reman" in read(claude))

    code, out = reman("uninstall", "--yes")
    print(out)
    check("--yes: it succeeds", code == 0, out)
    check("Claude Code: no reman left (MCP entry, hooks)", "reman" not in read(claude) and "reman" not in read(os.path.join(user, ".claude.json")), read(claude))
    check("Codex: no reman left (config.toml, hooks.json)", "reman" not in read(codex_cfg) and "reman" not in read(codex_hooks), read(codex_hooks))
    check("the HTTP endpoint is off", "http" not in json.load(open(cfg)))
    p = read(profile)
    check("the profile loses reman's block and keeps the rest", ">>> reman >>>" not in p and "Set-Alias ll ls" in p, p)
    b = read(bashrc)
    check("the .bashrc loses reman's line and keeps the rest", "reman" not in b and "export EDITOR=vim" in b, b)
    for _ in range(40):
        if not os.path.exists(bin_dir):
            break
        time.sleep(0.5)
    check("the program folder is deleted once reman exits", not os.path.exists(bin_dir))
    check("the history is kept", os.path.exists(os.path.join(home, "reman.db")) and os.path.exists(os.path.join(home, "models")))
    check("it says where the history is", "Your history is still in" in out, out)

    code, out = reman("uninstall", "--purge", "--yes", exe=os.path.join(BUILD, EXE_NAME))
    check("--purge deletes the history, settings and model too", code == 0 and not os.path.exists(home), out)


if __name__ == "__main__":
    sys.exit(main())
