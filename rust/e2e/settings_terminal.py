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
# the test plays the user: without the variables a coding agent sets (sharing refuses those)
ENV = dict({k: v for k, v in os.environ.items() if k not in ("CLAUDECODE", "GEMINI_CLI") and not k.startswith("CODEX_")},
           REMAN_CONNECT_HOME=box, REMAN_CONFIG=CFG)
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
check("the page opens with its sections", all(s in text for s in ("Coding tools", "What agents can see", "Privacy")))
check("a new user shares no folder: agents see no history until one is approved", "agents see no history" in text)
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

check("the strict-permissions switch is there, off by default", t.goto("Strict permissions") and "[ ]" in t.selected(), t.selected().strip())
t.keys("enter", wait=0.8)
check("Enter turns it on and saves", cfg().get("strict_permissions") is True and "[x]" in t.selected(), cfg())
t.keys("enter", wait=0.8)
check("Enter again turns it off (from the user's own terminal)", "strict_permissions" not in cfg(), cfg())

check("the strict-secrets switch can be selected", t.goto("Strict secrets"), t.selected().strip())
t.keys("enter", wait=0.8)
check("Enter flips it and saves", cfg().get("strict_secrets") is False)

check("the secrets row can be selected, masking by default", t.goto("Secrets in commands") and "mask the value" in t.selected(), t.selected().strip())
t.keys("enter", wait=0.8)
check("Enter: drop the command", cfg().get("secrets") == "drop" and "drop the command" in t.selected(), cfg().get("secrets"))
t.keys("enter", wait=0.8)
check("Enter: keep as typed (redaction off for you; agents still masked)", cfg().get("secrets") == "keep" and "agents still get them masked" in t.selected(), cfg().get("secrets"))
t.keys("enter", wait=0.8)
check("Enter: back to masking", "secrets" not in cfg() and "mask the value" in t.selected(), cfg().get("secrets"))

# Keys: a draft until s saves it. Press the new key (+ adds a second); a key that types is refused;
# Del turns one off; r gives it back; u undoes; leaving with changes unsaved asks first
def keys_cfg():
    return cfg().get("keys") or {}


check("Keys: the preset, standard at first", t.goto("Preset:", 80) and "standard" in t.selected(), t.selected().strip())
check("an action shows its short name and key", t.goto("Finder, every folder", 20) and "Ctrl+R" in t.selected(), t.selected().strip())
check("the help line says what it does in full", "Open the finder for every folder" in t.text(), t.text()[-400:])
t.keys("enter", wait=0.6)
check("Enter asks for the new key, naming the one it replaces", "Press the new key for Finder, every folder (now Ctrl+R)" in t.text(), t.text()[-300:])
t.keys("\x1bj", wait=0.8)
check("pressing Alt+J shows Alt+J, not saved yet", "Alt+J" in t.selected() and "not saved" in t.selected() and "find_all" not in keys_cfg(), t.selected().strip())
check("the footer offers s to save and u to undo", "save the keys" in t.text() and "undo" in t.text(), t.text()[-200:])
t.keys("+", wait=0.6)
check("+ asks for a second key, keeping the first", "Press a second key for Finder, every folder (Alt+J stays)" in t.text(), t.text()[-300:])
t.keys("\x1bl", wait=0.8)
check("...and adds it", "Alt+J, Alt+L" in t.selected(), t.selected().strip())
t.keys("enter", wait=0.6)
t.keys("x", wait=0.8)
check("a key that types is refused, with why", "types a character" in t.text() and "Press another" in t.text() and "Alt+J, Alt+L" in t.selected(), t.text()[-300:])
t.keys("\x03", wait=0.8)
check("still asking: a key the shell needs (Ctrl+C) is refused too", "your shell needs" in t.text() and "Alt+J, Alt+L" in t.selected() and t.p.isalive(), t.text()[-300:])
t.keys("esc", wait=0.8)
check("Esc while asking keeps the key", "kept as it was" in t.text() and "Alt+J, Alt+L" in t.selected() and t.p.isalive(), t.text()[-300:])
t.keys("\x1b[3~", wait=0.8)
check("Del turns it off (in the draft)", "off" in t.selected() and "find_all" not in keys_cfg(), t.selected().strip())
t.keys("r", wait=0.8)
check("r gives it the preset's key again (nothing left to save)", "Ctrl+R" in t.selected() and "not saved" not in t.selected(), t.selected().strip())
t.keys("enter", wait=0.6)
t.keys("\x1bj", wait=0.8)
t.keys("s", wait=1.0)
check("s saves it", keys_cfg().get("find_all") == ["Alt+J"] and "Saved" in t.text() and "not saved" not in t.selected(), cfg().get("keys"))
check("...and says where it applies", "open shells at their next prompt" in t.text(), t.text()[-300:])
t.keys("up", wait=0.4)
check("the row above: Finder, this folder", "Finder, this folder" in t.selected(), t.selected().strip())
t.keys("enter", wait=0.6)
t.keys("\x1b[B", wait=0.8)
check("...is taken, with a note that the shell's Down goes through history", "Down" in t.selected() and "Down no longer goes forward through history" in t.text(), t.text()[-300:])
check("Tab is on or off only", t.goto("Tab opens the finder", 10) and "Tab" in t.selected(), t.selected().strip())
t.keys("enter", wait=0.8)
check("Enter turns it off (in the draft)", "off" in t.selected() and "tab" not in keys_cfg(), t.selected().strip())
t.keys("u", wait=0.8)
check("u undoes both unsaved changes", "Tab" in t.selected() and "not saved" not in t.text() and keys_cfg() == {"find_all": ["Alt+J"]}, t.text()[-400:])
check("a finder key: every run is Ctrl+O", t.goto("Every run", 20) and "Ctrl+O" in t.selected(), t.selected().strip())
t.keys("enter", wait=0.6)
t.keys("\x10", wait=0.8)
check("a key another finder action has (Ctrl+P, pin) is refused, naming it", "already the key for Pin" in t.text() and "Ctrl+O" in t.selected(), t.text()[-300:])
t.keys("esc", wait=0.6)
while "Preset:" not in t.selected():
    t.keys("up", wait=0.1)
t.keys("enter", wait=0.8)
check("the preset cycles to gentle, your key change dropped (in the draft)", "gentle" in t.selected() and "dropped" in t.text() and "key_preset" not in cfg(), t.text()[-300:])
t.keys("esc", wait=0.8)
check("leaving with unsaved keys asks first", t.p.isalive() and "aren't saved" in t.text(), t.text()[-300:])
t.keys("s", wait=1.0)
check("s saves the preset", cfg().get("key_preset") == "gentle" and not cfg().get("keys"), cfg())
t.keys("enter", wait=0.8)
t.keys("enter", wait=0.8)
t.keys("s", wait=1.0)
check("...vim, then standard again, saved", "key_preset" not in cfg() and "finder_keys" not in cfg(), cfg())

check("the language-model switch is there, on by default", t.goto("Use a language model") and "[x]" in t.selected(), t.selected().strip())
check("it says where the model comes from", "auto: a model running on this machine" in t.text())
t.keys("enter", wait=0.8)
check("Enter turns it off and saves", (cfg().get("ai") or {}).get("enabled") is False, cfg().get("ai"))
check("further down, the Shells section (the page scrolls)", t.goto("PowerShell") and "Shells" in t.text(), t.selected().strip())
t.snap("after the changes")

t.keys("esc", wait=1.0)
check("Esc closes the page", not t.p.isalive())

# a change left unsaved: Esc twice leaves without it
t = Term()
time.sleep(3)
t.goto("Finder, every folder", 80)
t.keys("enter", wait=0.6)
t.keys("\x1bm", wait=0.8)
t.keys("esc", wait=0.8)
check("Esc with a change unsaved asks", t.p.isalive() and "aren't saved" in t.text(), t.text()[-300:])
t.keys("esc", wait=1.0)
check("a second Esc leaves without it", not t.p.isalive() and "find_all" not in (cfg().get("keys") or {}), cfg())
shutil.rmtree(box, ignore_errors=True)
print("\nSETTINGS RESULT:", "PASS" if all(results) else f"FAIL ({results.count(False)})")
