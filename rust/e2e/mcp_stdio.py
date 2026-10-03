"""test_mcp_protocol.py, pointed at the Rust server: spawn `reman.exe mcp` and drive it over the
real MCP stdio protocol (initialize -> list_tools -> call_tool) exactly as an agent would."""
import asyncio, os, sys
from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client

EXE = os.environ.get("REMAN_EXE_UNDER_TEST") or os.path.join(os.path.expanduser("~"), ".reman", "bin", "reman.exe")
ENV = dict(os.environ)  # roots come from ~/.reman/config.json (reman connect --root)


async def main():
    params = StdioServerParameters(command=EXE, args=["mcp"], env=ENV)
    async with stdio_client(params) as (read, write):
        async with ClientSession(read, write) as session:
            await session.initialize()
            tools = await session.list_tools()
            names = [t.name for t in tools.tools]
            print("MCP handshake OK. tools advertised:", names)

            r = await session.call_tool("reman_search", {"intent": "run database migrations", "worked_only": False, "k": 3})
            text = r.content[0].text
            print("\nreman_search('run database migrations') over MCP ->")
            print(text[:600])

            r2 = await session.call_tool("reman_fixes", {"failed_command": "dcoker ps"})
            print("\nreman_fixes('dcoker ps') over MCP ->")
            print(r2.content[0].text[:300])

            r3 = await session.call_tool("reman_recent", {"cwd": r"C:\Windows\System32"})
            denied = r3.content[0].text.strip() == "[]"
            print("\nreman_recent(cwd outside root) ->", r3.content[0].text.strip()[:80])

            ok = ({"reman_search", "reman_recent", "reman_fixes", "reman_check", "reman_flows", "reman_failures", "reman_next"} <= set(names)
                  and "alembic" in text and '"generated": false' in text and denied)
            print("\nMCP over-the-wire AC (rust):", "PASS" if ok else "FAIL")
            return ok  # exit outside the client: SystemExit inside it becomes an ExceptionGroup


sys.exit(0 if asyncio.run(main()) else 1)
