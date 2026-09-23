# reman (Rust)

Semantic, provenance-aware shell history for you and your AI agents. It is one binary with no Atuin and no Python.

## Install / cut over

Build from PowerShell. On a machine without the Windows SDK, run `. .\msvc-env.ps1` first.

```powershell
cargo build --release
.\target\release\reman.exe setup
.\target\release\reman.exe connect all
```

`setup` runs these steps:
1. Installs `reman` and the slim `reman-hook` into `~/.reman/bin`, so a running daemon never locks your build output.
2. Starts the warm daemon.
3. Imports Atuin history one last time.
4. Removes double captures.
5. Backfills fix-pairs.
6. Rewires your PowerShell profile. A backup is kept as `*.reman-bak`.

For bash, zsh and fish, add this to your rc file instead: `eval "$(reman init zsh)"` (or `reman init fish | source`).

## Keys

These keys work in PowerShell, bash, zsh and fish.

| key | action |
|---|---|
| `UpArrow` | open the finder, scoped to this folder |
| `Ctrl+R` | open the finder across all folders |
| `Tab` | normal completion. When there is nothing to complete (PowerShell) or the line is empty (every shell), it opens the finder with what you typed |
| `Alt+F` | insert the fix suggested after a failed command |

Inside the finder:

| finder key | action |
|---|---|
| `Ctrl+T` | switch tab: **Recall**, **Fixes**, **Flows** |
| `←` / `→` | scope: folder → repo → all |
| `Tab` | actor: all → you → agent |
| `F2` | pass/fail filter |
| `Ctrl+G` | group variants |
| `Ctrl+P` | pin |
| `Del` `Del` | forget the command everywhere |
| `Enter` | insert the command |
| `Esc` | close |

With an empty query, the finder shows **predicted next commands** first. reman's own invocations (`reman search …`) are recorded but never offered back, unless your query mentions reman.

## Capture cost

This is the cost per command, measured in real terminals by `e2e/posix_shells_terminal.py`:

| shell | how it reaches the daemon | cost |
|---|---|---|
| PowerShell | in-process .NET socket | no process spawn |
| bash | `/dev/tcp`, fork-free (builtins only) | ~5 ms |
| zsh | `zsh/net/tcp` | ~2 ms |
| fish | append to the spool file with builtins; the daemon drains it every 1 s | ~4 ms |
| Claude Code hook / fish failures | `reman-hook` (1.9 MB, no ONNX Runtime) | ~Windows process-spawn floor |

## Agents: plug and play

```
reman connect                      status of every agent on this machine
reman connect all                  connect every installed agent
reman connect claude-code codex    connect specific ones
reman connect --root D:\proj ...   which folders agents may see (one list, applied to every agent)
reman connect http [--port 8777]   local HTTP endpoint for SDKs / scripts
reman connect --print              config to paste into any other MCP client
reman disconnect <id> | all | http
```

| agent | what `connect` writes |
|---|---|
| Claude Code | MCP server via `claude mcp add -s user`, plus capture hooks in `~/.claude/settings.json` (Bash runs are recorded as `agent:claude-code`) |
| Claude Desktop | `mcpServers.reman` in `claude_desktop_config.json` |
| OpenAI Codex CLI | `[mcp_servers.reman]` in `~/.codex/config.toml` |
| Cursor | `~/.cursor/mcp.json` |
| VS Code (Copilot agent mode) | `servers.reman` in the user `mcp.json` |
| Windsurf | `~/.codeium/windsurf/mcp_config.json` |
| Gemini CLI | `mcpServers.reman` in `~/.gemini/settings.json` |

`connect` only edits files it can parse. Files with comments (JSONC) are left alone, and it prints the snippet for you to add. It keeps a `.reman-bak` backup and is idempotent. `disconnect` removes exactly what was added.

### Other agents and SDKs: HTTP and function calling

After `reman connect http`, the daemon serves these routes on `127.0.0.1` only. Every request needs a bearer token, which is in `~/.reman/config.json`. Requests that carry a non-local `Origin` are refused, which protects against browser and DNS-rebinding attacks.

| route | purpose |
|---|---|
| `POST /mcp` | MCP Streamable HTTP. For example, the OpenAI Agents SDK: `MCPServerStreamableHttp(params={"url": "http://127.0.0.1:8777/mcp", "headers": {"Authorization": "Bearer <token>"}})` |
| `GET /tools?format=openai\|openai-responses\|anthropic\|mcp` | ready-made function-calling schemas |
| `POST /tools/<name>` | call a tool; the JSON body is its arguments |

Without HTTP, you can also use the CLI: `reman tools --format openai` for the schemas, and `reman call reman_search '{"intent":"run the tests"}'` to call a tool.

**Security boundary.** Agents only see commands whose recorded folder is inside the shared roots. Secrets are redacted, and in strict mode anything still secret-looking is withheld. A result must be relevant: an agent gets an empty answer rather than an unrelated "closest" command.

## Commands

```
reman search <intent>        hybrid semantic + fuzzy search (--here, --semantic for python parity)
reman fixes <failed cmd>     proven fixes first, then did-you-mean
reman next                   what you usually run next here
reman flows [--here]         recurring command sequences
reman check <cmd>            verified / failed / mixed / never_run
reman stats | doctor | bench
reman import atuin|psreadline|bash|zsh|fish
reman forget <cmd> | pin <cmd> [--off] | export [--here --actor --status --query]
reman mcp                    MCP stdio server (7 tools incl. reman_next)
```

## Storage

Everything lives in `~/.reman/`:

| path | what |
|---|---|
| `reman.db` | SQLite, same schema as the Python version |
| `config.json` | agent roots, strict-secrets flag, HTTP port/token |
| `spool.jsonl` | fish captures and runs captured while the daemon was down, drained every second |
| `models/` | bge-small-en-v1.5 (ONNX) |
| `daemon.log`, `tui-panic.log` | logs |

To keep a command out of history, start it with a leading space, or match it with the `REMAN_IGNORE=<regex>` environment variable.

## Tests

- `cargo test`: unit tests.
- `e2e/`: real-terminal suites for PowerShell, bash, zsh and fish, plus the HTTP and stdio agent suites. See `e2e/README.md`.
