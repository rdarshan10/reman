import json
import reman_mcp as M

def show(title, data, cap=1400):
    print(f"\n=== {title} ===")
    print(json.dumps(data, indent=2, default=str)[:cap])

# AC: agent asks to deploy -> gets the user's REAL deploy commands (worked_only on real data;
# seed has empty success pool so we pass worked_only=False to draw from history)
show("reman.search('deploy the app to the server')",
     M.reman_search("deploy the app to the server", worked_only=False, k=3))
show("reman.recent(n=5)", M.reman_recent(n=5), cap=900)
show("reman.fixes('dcoker ps')", M.reman_fixes("dcoker ps"))

# HARD CONTRACT: nothing generated, every result is a real command
allres = (M.reman_search("deploy", worked_only=False, k=5)
          + M.reman_recent(n=5) + M.reman_fixes("gti pull"))
violations = [r for r in allres if r.get("generated") is not False]
print(f"\nCONTRACT: {len(allres)} results, generated-command violations = {len(violations)}",
      "-> OK (all real)" if not violations else "-> FAIL")

# server registers the three tools
srv = M._build_server()
try:
    tools = srv._tool_manager.list_tools()
    print("MCP server tools registered:", sorted(t.name for t in tools))
except Exception as e:
    print("server built (tool-list introspection skipped:", type(e).__name__, ")")
