"""The HTTP connector, driven by real clients: OpenAI Agents SDK (MCP Streamable HTTP), plain REST,
and the security checks."""
import asyncio, json, os, sys, urllib.request, urllib.error
sys.stdout.reconfigure(encoding="utf-8")
cfg = json.load(open(os.path.join(os.path.expanduser("~"), ".reman", "config.json"), encoding="utf-8"))
TOKEN, PORT = cfg["http"]["token"], cfg["http"]["port"]
BASE = f"http://127.0.0.1:{PORT}"
REPO = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", ".."))
results = []


def check(name, ok, detail=""):
    results.append(ok)
    print(f"  {'ok  ' if ok else 'FAIL'} {name}" + (f"   [{detail}]" if detail and not ok else ""))


def http(method, path, body=None, headers=None):
    h = {"Content-Type": "application/json", **(headers or {})}
    req = urllib.request.Request(BASE + path, data=json.dumps(body).encode() if body is not None else None, headers=h, method=method)
    try:
        with urllib.request.urlopen(req, timeout=20) as r:
            return r.status, json.loads(r.read() or b"null")
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"null")


AUTH = {"Authorization": f"Bearer {TOKEN}"}
print("== security")
check("health is public", http("GET", "/health")[0] == 200)
check("no token -> 401", http("POST", "/tools/reman_recent", {})[0] == 401)
check("wrong token -> 401", http("POST", "/tools/reman_recent", {}, {"Authorization": "Bearer nope"})[0] == 401)
check("foreign Origin (browser / DNS rebinding) -> 403", http("POST", "/tools/reman_recent", {}, {**AUTH, "Origin": "https://evil.example"})[0] == 403)
check("localhost Origin allowed", http("POST", "/tools/reman_recent", {"n": 1}, {**AUTH, "Origin": "http://localhost:3000"})[0] == 200)
st, out = http("POST", "/tools/reman_recent", {"cwd": r"C:\Windows", "n": 5}, AUTH)
check("folder outside the shared roots -> nothing", st == 200 and out == [], out)

print("== plain function calling (REST)")
st, oa = http("GET", "/tools?format=openai", headers=AUTH)
check("OpenAI function schemas", st == 200 and len(oa) == 8 and oa[0]["type"] == "function" and "parameters" in oa[0]["function"])
st, an = http("GET", "/tools?format=anthropic", headers=AUTH)
check("Anthropic tool schemas", st == 200 and "input_schema" in an[0])
st, res = http("POST", "/tools/reman_search", {"intent": "run database migrations", "worked_only": False, "k": 3}, AUTH)
check("POST /tools/reman_search returns real commands", st == 200 and res and all(r["generated"] is False for r in res), res)
print("     ->", [r["command"][:60] for r in res])

print("== official MCP Python SDK: streamablehttp_client (what the OpenAI Agents SDK wraps)")


async def mcp_sdk():
    from mcp import ClientSession
    import mcp.client.streamable_http as sh
    if hasattr(sh, "streamablehttp_client"):          # mcp <= 1.2x
        ctx = sh.streamablehttp_client(f"{BASE}/mcp", headers=AUTH)
    else:                                              # newer mcp: pass a configured httpx client
        import httpx
        ctx = sh.streamable_http_client(f"{BASE}/mcp", http_client=httpx.AsyncClient(headers=AUTH, timeout=20))
    async with ctx as (read, write, *_):
        async with ClientSession(read, write) as s:
            await s.initialize()
            names = [t.name for t in (await s.list_tools()).tools]
            check("MCP SDK connects + lists the 8 reman tools", len(names) == 8 and "reman_search" in names, names)
            r = await s.call_tool("reman_check", {"command": "git status"})
            text = r.content[0].text
            check("MCP SDK tool call (reman_check) returns a verdict", '"verdict"' in text, text[:200])
            print("     ->", json.loads(text)["verdict"], "|", json.loads(text).get("advice", "")[:80])


asyncio.run(mcp_sdk())

print("== OpenAI Agents SDK: MCPServerStreamableHttp")


async def agents_sdk():
    from agents.mcp import MCPServerStreamableHttp
    server = MCPServerStreamableHttp(params={"url": f"{BASE}/mcp", "headers": AUTH}, name="reman", client_session_timeout_seconds=20)
    async with server:
        names = [t.name for t in await server.list_tools()]
        check("Agents SDK connects + lists the 8 reman tools", len(names) == 8, names)
        r = await server.call_tool("reman_next", {"cwd": REPO})
        err = getattr(r, "is_error", None)
        err = getattr(r, "isError", False) if err is None else err
        check("Agents SDK tool call (reman_next) works", bool(r.content) and not err, r)
        print("     ->", r.content[0].text[:160].replace("\n", " "))


try:
    asyncio.run(agents_sdk())
except ImportError as e:
    print("  skip Agents SDK not importable on this Python:", e)
print("\nHTTP RESULT:", "PASS" if all(results) else "FAIL")
