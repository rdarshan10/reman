# End-to-end suites (real terminals, real agent SDKs)

These suites drive the **installed** binaries (`~/.reman/bin`) the way a person or an agent would.
The terminal suites run on an isolated daemon (port 8768) against a *copy* of `~/.reman/reman.db`, so your real history is never touched.

| suite | what it drives | needs |
|---|---|---|
| `cmd_terminal.py` | a real Command Prompt with Clink. It checks capture with the real `%ERRORLEVEL%`, that `if errorlevel` still works, text cmd can't quote, the fix suggestion and Alt+F, the finder on ↑ and Ctrl+R, and Tab completion for reman | Clink, plus `reman setup` |
| `powershell_wrappers.py` | exit-code capture when something wraps the prompt after reman: a Python venv's `Activate.ps1` (and `deactivate`), VS Code's shell integration. Also checks that open shells reload reman after an update | the above, plus a venv at `<repo>/.venv` |
| `powershell_terminal.py` | a real interactive Windows PowerShell in ConPTY that loads your profile. It presses the actual keys (UpArrow, Ctrl+R, Tab, Alt+F, Del, F2, Ctrl+T) and reads the screen through a VT emulator | `pip install pywinpty pyte` |
| `posix_shells_terminal.py` | bash (Git Bash), zsh and fish (MSYS2) through their `reman init <shell>` integrations. It checks capture, exit codes, suggestions, Alt-F, the finder, Tab, and per-command capture cost | the above, plus `pacman -S zsh fish` in MSYS2 |
| `http_agents.py` | the HTTP connector: auth / Origin / boundary checks, REST function calling, the official MCP SDK, and the OpenAI Agents SDK | `reman connect http`; to cover the Agents SDK, run it as `uv run --python 3.12 --with openai-agents --with mcp --with httpx python e2e/http_agents.py` |
| `mcp_stdio.py` | `reman mcp` over stdio with the official MCP client (the path Claude Code, Cursor and others use) | `pip install mcp` |

To run a suite: `python e2e/<suite>.py`. Each prints `ok` or `FAIL` per check and a final `RESULT`.
