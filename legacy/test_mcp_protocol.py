"""Real end-to-end MCP test: spawn reman_mcp.py as a subprocess and drive it over the actual
MCP stdio protocol (initialize -> list_tools -> call_tool), exactly as an AI agent would.
This is the true Phase 6 validation - not an in-process function call."""
import asyncio, sys
from mcp import ClientSession, StdioServerParameters
from mcp.client.stdio import stdio_client

PY = r"C:\Users\dev\reman\.venv\Scripts\python.exe"
SERVER = r"C:\Users\dev\reman\reman_mcp.py"


async def main():
    params = StdioServerParameters(command=PY, args=[SERVER])
    async with stdio_client(params) as (read, write):
        async with ClientSession(read, write) as session:
            await session.initialize()
            tools = await session.list_tools()
            names = [t.name for t in tools.tools]
            print("MCP handshake OK. tools advertised:", names)

            r = await session.call_tool("reman_search",
                                        {"intent": "run database migrations", "worked_only": False, "k": 3})
            text = r.content[0].text
            print("\nreman_search('run database migrations') over MCP ->")
            print(text[:600])

            r2 = await session.call_tool("reman_fixes", {"failed_command": "dcoker ps"})
            print("\nreman_fixes('dcoker ps') over MCP ->")
            print(r2.content[0].text[:300])

            ok = ("reman_search" in names and "reman_recent" in names and "reman_fixes" in names
                  and "alembic" in text and '"generated": false' in text)
            print("\nPHASE 6 over-the-wire AC:", "PASS" if ok else "FAIL")
            sys.exit(0 if ok else 1)


asyncio.run(main())
