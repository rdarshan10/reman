"""Phase 4 on REAL data: run fix-pair detection over the executions actually captured by the
live Claude Code hook (real exit codes + session), not a simulation."""
import sqlite3, os
from collections import defaultdict
from reman_fixpairs import detect_in_session

db = sqlite3.connect(os.path.expanduser("~/.reman/reman.db"))

print("recent captured executions (real exit codes from the hook):")
for r in db.execute("""SELECT e.exit, e.session, c.cmd_text
                       FROM executions e JOIN commands c ON c.id=e.command_id
                       ORDER BY e.id DESC LIMIT 8"""):
    print(f"  exit={r[0]}  session={(r[1] or '')[:8]}  {r[2]}")

sessions = defaultdict(list)
for eid, sess, ex, cmd in db.execute("""SELECT e.id, e.session, e.exit, c.cmd_text
                                        FROM executions e JOIN commands c ON c.id=e.command_id
                                        ORDER BY e.session, e.ts, e.id"""):
    sessions[sess].append({"pos": eid, "cmd": cmd, "exit": ex if ex is not None else 0})

pairs = []
for sess, events in sessions.items():
    pairs.extend(detect_in_session(events))

print("\nfix-pairs detected from REAL captured executions:")
for p in pairs:
    print(f"  [{p['confidence']}] {p['failed_text']!r} -> {p['fixed_cmd']!r}  (error_tail={p['error_tail']})")
if not pairs:
    print("  (none detected — need a real fail->fix sequence in the captured executions)")
