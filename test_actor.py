import os, tempfile, sqlite3, json, io, sys

# throwaway DB so the real reman.db is never touched
TMP = tempfile.mktemp(suffix="_actor.db")
os.environ["REMAN_DB"] = TMP

import reman
reman.connect()                       # create base schema (commands + command_vec) at TMP
from reman_actor import detect_actor, migrate_actor, record_run, actor_log
from reman_enrich import migrate as enrich_migrate
db = sqlite3.connect(TMP)
migrate_actor(db)
enrich_migrate(db)                     # adds description/desc cols + command_desc_vec that search expects

fails = []

# ---- T7.1: ActorDetector ----
t71 = {
    "CLAUDECODE=1 -> claude-code": detect_actor({"CLAUDECODE": "1"}) == "agent:claude-code",
    "AGENT=goose convention":      detect_actor({"AGENT": "goose"}) == "agent:goose",
    "AGENT=future-agent generic":  detect_actor({"AGENT": "newthing"}) == "agent:newthing",
    "CODEX_CI holdout":            detect_actor({"CODEX_CI": "1"}) == "agent:codex",
    "no marker -> human":          detect_actor({"PATH": "/usr/bin"}) == "human",
}
print("== T7.1 ActorDetector ==")
for k, v in t71.items():
    print(f"  {'ok ' if v else 'FAIL'} {k}")
if not all(t71.values()): fails.append("T7.1")

# ---- T7.2: Claude Code PostToolUse hook (adapter #3) ----
payload = {"tool_name": "Bash", "tool_input": {"command": "alembic upgrade head"},
           "cwd": "D:\\PlanetNaidu", "session_id": "sess-abc",
           "tool_response": {"exit_code": 0}}
import reman_hook_claude
sys.stdin = io.StringIO(json.dumps(payload))
reman_hook_claude.main()
sys.stdin = sys.__stdin__
row = db.execute("SELECT last_actor, last_exit, cwd, agent_runs FROM commands WHERE cmd_text='alembic upgrade head'").fetchone()
t72 = row == ("agent:claude-code", 0, "D:\\PlanetNaidu", 1)
print("\n== T7.2 Claude Code hook ==")
print(f"  recorded: {row}")
print(f"  {'ok ' if t72 else 'FAIL'} agent command captured with actor + exit + cwd (Atuin never saw it)")
if not t72: fails.append("T7.2")

# ---- T7.3: actor counts + log filter ----
record_run(db, "git status", 0, "D:\\PlanetNaidu", "h1", "human")
record_run(db, "eas build --platform android", 0, "D:\\app", "a1", "agent:claude-code")
record_run(db, "npm run typecheck", 1, "D:\\app", "h2", "human")
human = [r["command"] for r in actor_log(db, "human")]
agent = [r["command"] for r in actor_log(db, "agent")]
cc    = [r["command"] for r in actor_log(db, "agent:claude-code")]
t73 = ("git status" in human and "npm run typecheck" in human
       and "alembic upgrade head" not in human                       # agent cmd not in human
       and set(agent) == {"alembic upgrade head", "eas build --platform android"}
       and "alembic upgrade head" in cc)
print("\n== T7.3 log --actor filter ==")
print(f"  --actor human: {human}")
print(f"  --actor agent: {agent}")
print(f"  --actor agent:claude-code: {cc}")
print(f"  {'ok ' if t73 else 'FAIL'} filters partition correctly")
if not t73: fails.append("T7.3")

# ---- T7.4: MCP actor-weighting (prefer='human') ----
record_run(db, "pytest", 0, "D:\\app", "h3", "human")              # human-verified success
record_run(db, "cargo test --all", 0, "D:\\app", "a2", "agent:claude-code")  # agent-run success
import reman_mcp
res = reman_mcp.reman_search("run the test suite", worked_only=False, k=8, prefer="human")
order = [r["command"] for r in res]
def idx(c): return order.index(c) if c in order else 10**6
t74 = idx("pytest") < idx("cargo test --all")        # human-run outranks agent-run
print("\n== T7.4 MCP prefer='human' ==")
for r in res:
    if r["command"] in ("pytest", "cargo test --all"):
        print(f"  #{order.index(r['command'])}: {r['command']:<20} actor={r.get('actor')} "
              f"human={r.get('human_runs')} agent={r.get('agent_runs')}")
print(f"  {'ok ' if t74 else 'FAIL'} human-verified 'pytest' outranks agent-run 'cargo test --all'")
if not t74: fails.append("T7.4")

print("\n" + ("PHASE 7 AC PASS - all of T7.1-T7.4" if not fails else f"FAIL: {fails}"))
try: os.remove(TMP)
except Exception: pass
sys.exit(0 if not fails else 1)
