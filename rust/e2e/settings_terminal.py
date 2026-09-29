"""Drive `reman settings` in a real terminal (ConPTY), against a sandbox: coding-tool configs live in
a temp folder (REMAN_CONNECT_HOME) and reman's own settings in a temp file (REMAN_CONFIG), so your
real setup is never touched. Toggles a tool on and off, shares a suggested folder, types in a folder,
flips a privacy switch, and checks each change landed in the files.
REMAN_EXE_UNDER_TEST points it at a build other than the installed one."""
import json, os, shutil, sys, tempfile, threading, time
import winpty, pyte

sys.stdout.reconfigure(encoding="utf-8")
ROWS, COLS = 40, 120
EXE = os.environ.get("REMAN_EXE_UNDER_TEST") or os.path.join(os.path.expanduser("~"), ".reman", "bin", "reman.exe")
box = tempfile.mkdtemp(prefix="reman-settings-")
for d in (".claude", ".cursor", ".gemini"):
    os.makedirs(os.path.join(box, d), exist_ok=True)
CFG = os.path.join(box, "reman-config.json")
ENV = dict(os.environ, REMAN_CONNECT_HOME=box, REMAN_CONFIG=CFG)
SHARE_TYPED = os.path.dirname(os.path.abspath(__file__))   # e2e folder: exists, never in history
BAR = " ▌ "   # the selection bar


class Term:
    def __init__(self):
        self.screen = pyte.Screen(COLS, ROWS)
        self.stream = pyte.Stream(self.screen)
        self.lock = threading.Lock()
        self.p = winpty.PtyProcess.spawn([EXE, "settings"], env=ENV, dimensions=(ROWS, COLS))
        threading.Thread(target=self._pump, daemon=True).start()

    def _pump(self):
        while True:
            try:
                d = self.p.read(65536)
            except EOFError:
                return
            if "\x1b[c" in d:
                self.p.write("\x1b[?61;4;6;7;14;21;22;23;24;28;32;42c")
            with self.lock:
                self.stream.feed(d)

    def text(self):
        with self.lock:
            return "\n".join(l.rstrip() for l in self.screen.display)

    def selected(self):
        s = [l for l in self.text().splitlines() if l.startswith(BAR)]
        return s[0] if s else ""

    def keys(self, *ks, wait=0.4):
        for k in ks:
            self.p.write({"down": "\x1b[B", "up": "\x1b[A", "enter": "\r", "esc": "\x1b", "tab": "\t"}.get(k, k))
            time.sleep(wait)

    def goto(self, needle, limit=40):
        """Move the selection down until the selected row contains `needle`."""
        for _ in range(limit):
            if needle in self.selected():
                return True
            self.keys("down", wait=0.15)
        return needle in self.selected()

    def snap(self, title):
        print(f"\n----- {title} " + "-" * max(0, COLS - 8 - len(title)))
        print("\n".join(l.rstrip() for l in self.screen.display if l.strip()))


results = []


def check(name, ok, detail=""):
    results.append(ok)
    print(f"  {'ok  ' if ok else 'FAIL'} {name}" + (f"  ({detail})" if detail else ""))


def cfg():
    return json.load(open(CFG)) if os.path.exists(CFG) else {}


t = Term()
time.sleep(3)
t.snap("reman settings (sandbox)")
text = t.text()
check("the page opens with its sections", all(s in text for s in ("Coding tools", "What agents can see", "Privacy", "Shells")))
check("a new user shares no folder: agents see only their own project", "each agent sees only the project it's working in" in text)
check("installed tools are listed as not connected yet", "Claude Code" in text and "installed, not connected" in text)

check("the first row is Claude Code", "Claude Code" in t.selected(), t.selected().strip())
t.keys("enter", wait=2.5)
cj = os.path.join(box, ".claude.json")
check("Enter connects it (MCP entry + capture hooks written)",
      os.path.exists(cj) and "reman" in json.load(open(cj)).get("mcpServers", {})
      and os.path.exists(os.path.join(box, ".claude", "settings.json")))
check("the row now says connected", "connected" in t.selected() and "not connected" not in t.selected(), t.selected().strip())
t.keys("enter", wait=2.5)
check("Enter again disconnects it", "reman" not in json.load(open(cj)).get("mcpServers", {}))

check("a suggested folder can be selected", t.goto("not shared"), t.selected().strip())
folder = t.selected().split("[ ]")[1].strip().split("  ")[0].rstrip("…")
t.keys("enter", wait=1.0)
roots = cfg().get("mcp_roots", [])
check("Enter shares it", any(r.lower().startswith(folder.lower()) for r in roots), f"{folder} -> {roots}")

t.keys("a", wait=0.4)
for ch in SHARE_TYPED:
    t.p.write(ch)
    time.sleep(0.01)
t.keys("enter", wait=1.0)
roots = cfg().get("mcp_roots", [])
check("`a` + a typed path shares that folder", any(os.path.normcase(r) == os.path.normcase(SHARE_TYPED) for r in roots), str(roots))

check("the strict-secrets switch can be selected", t.goto("Strict secrets"), t.selected().strip())
t.keys("enter", wait=0.8)
check("Enter flips it and saves", cfg().get("strict_secrets") is False)

check("the language-model switch is there, on by default", t.goto("Use a language model") and "[x]" in t.selected(), t.selected().strip())
check("it says where the model comes from", "auto: a model running on this machine" in t.text())
t.keys("enter", wait=0.8)
check("Enter turns it off and saves", (cfg().get("ai") or {}).get("enabled") is False, cfg().get("ai"))
t.snap("after the changes")

t.keys("esc", wait=1.0)
check("Esc closes the page", not t.p.isalive())
shutil.rmtree(box, ignore_errors=True)
print("\nSETTINGS RESULT:", "PASS" if all(results) else f"FAIL ({results.count(False)})")
