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

**Command Prompt (cmd.exe)** works without add-ons, through two short commands:

| type | what happens |
|---|---|
| `r` | the finder, for this folder. `r docker` starts it with "docker" already typed |
| `rr` | the finder, for all folders |

The command you pick is typed onto your next prompt, ready to edit or run with Enter; nothing runs by itself. `reman setup` defines `r` and `rr` as DOSKEY macros (`~/.reman/cmd-macros.txt`) and has cmd load them at startup through its AutoRun setting. Anything else already in AutoRun keeps running.

That's all plain cmd.exe allows. It has no hook that runs after a command and no way to bind keys, so in Command Prompt:
- commands aren't recorded automatically;
- there are no exit codes or fix suggestions;
- ↑ stays cmd's own history.

Your recorded history from the other shells is all there in `r`.

**If you use [Clink](https://chrisant996.github.io/clink/)**, `reman setup` wires reman into it instead (`reman init cmd`). That gives Command Prompt everything the other shells have: recording with the real `%ERRORLEVEL%`, fix suggestions, ↑ / Ctrl+R / Alt+F, and Tab completion.

## Keys

These keys work in PowerShell, bash, zsh and fish (and in Command Prompt with Clink; plain Command Prompt uses `r` / `rr`, above).

| key | action |
|---|---|
| `UpArrow` | open the finder, scoped to this folder |
| `Ctrl+R` | open the finder across all folders |
| `Tab` | normal completion, including `reman`'s own commands (see below). When there is nothing to complete (PowerShell) or the line is empty (every shell), it opens the finder with what you typed |
| `Alt+F` | insert the fix suggested after a failed command |

### The finder

The finder works like a command palette, bottom-up like the prompt it replaces:
- The query is the last line, and the tabs **Recall · Fixes · Flows** sit at its right.
- The filters sit just above the query and read as a sentence: *this folder first · by anyone · any outcome*. Each filter shows the key that changes it.
- The best result is nearest the prompt, and `↑` moves further back.
- Results come in labelled sections; a section's label sits at the base of its stack.
- A card beside the list (or above it, on narrow terminals) explains the selected command: what it does, whether it worked, where it ran, and why it matched.
- The top line shows only the keys that matter right now.

| when | what you see |
|---|---|
| you're walking through a flow | **flow in progress · 1 of 2 done**: the next step is selected, right above the prompt |
| your last command just failed | **last command failed: gti status** → `git status` (a proven fix, or the closest command that worked) |
| empty query | **likely next**: what you usually run after your last command, then **recent in this folder**. An agent's one-off exploration (`cd x && grep …`) is hidden, with a note saying how many; `F3` shows it |
| you type | **this folder** (strong matches only), then **everywhere**, ranked by meaning plus text. A folder with no good match never dead-ends |
| Fixes tab | commands that failed. The card shows *what worked instead*, and `Enter` inserts that fix |
| Flows tab | step sequences you repeat (`a → b → c`) |

**Walking through a flow.** In Flows:
1. `Enter` opens a flow and lists its steps, each with its own stats.
2. `Enter` on a step puts that step on your prompt and queues the rest.
3. After the step runs, the next `↑` offers the following step first, and so on to the end.

Steps you skip ahead to count too. `Ctrl+X` stops the flow, and it expires after 30 idle minutes. `Ctrl+A` inserts the whole flow as one line instead.

Each tab keeps its own query, so switching tabs never carries "run migrations" into Fixes.

| finder key | action |
|---|---|
| `↑` / `↓`, `PgUp` / `PgDn`, mouse wheel | move (`↑` goes further back) |
| `Enter`, or a click on the selected row | put the command on your prompt (it does not run) |
| `Tab` / `Shift+Tab`, `Ctrl+T` | next / previous tab: **Recall → Fixes → Flows** |
| `Alt+1` `Alt+2` `Alt+3`, or a click on a tab | jump straight to a tab |
| `←` / `→` | where: this folder → this repo → everywhere (in an open flow, `←` goes back) |
| `F3` | who: anyone → you → agents |
| `F2` | outcome: any → worked → failed |
| `Ctrl+G` | fold variants that differ only in data (messages, paths, ids) / show each |
| `Ctrl+P` | pin |
| `Del` `Del` | forget the command everywhere |
| `Ctrl+A` | Flows: insert every step as one line |
| `Ctrl+X` | stop the flow in progress |
| `F1` | all keys |
| `Esc` | back (out of an open flow) / close |

What never gets offered back:
- reman's own invocations (`reman search …`), unless your query mentions reman;
- lines of source code that PowerShell recorded when you pasted code into a prompt (`return db_obj`, `for i in range(n):`). These stay in the database but are hidden.

A command that only ever failed never leads a list.

**Rendering:** colours are named ANSI colours, so light and dark themes both read well. Every changed line is repainted whole and cleared to the end, so leftover fragments can't survive a redraw.

### Tab completion for `reman` itself

`reman init <shell>` wires Tab completion for `reman`'s own commands. All four shells use one engine, `reman complete`:

| you type | Tab offers |
|---|---|
| `reman co` | `connect` (subcommands, with their help) |
| `reman connect ` | `all`, `http`, and each agent with its state: *Claude Code - connected*, *Cursor - not installed* |
| `reman connect --add-root ` | your busiest folders that agents can't see yet, then folders on disk |
| `reman connect --remove-root ` | the folders agents see now |
| `reman forget dock` / `pin` / `check` | your own commands that match, e.g. `'docker ps'` (quoted, so it stays one argument) |
| `reman fixes ` | your failed commands |
| `reman call ` | the agent tools with what each does |
| `reman init ` / `import ` / `tools --format ` | shells / history sources / schema formats |
| `reman connect --old` | `--old-history`; flags are offered once you type `-` |

- **PowerShell:** a `reman …` line opens PowerShell's menu of choices, with the highlighted choice's description shown below it.
- **zsh and fish:** descriptions show next to each choice.
- **bash:** plain choices.

Completion never starts the daemon: if it isn't running, only the live values are skipped.

## Capture cost

This is the cost per command, measured in real terminals by `e2e/posix_shells_terminal.py`:

| shell | how it reaches the daemon | cost |
|---|---|---|
| PowerShell | in-process .NET socket | no process spawn |
| bash | `/dev/tcp`, fork-free (builtins only) | ~5 ms |
| zsh | `zsh/net/tcp` | ~2 ms |
| fish | append to the spool file with builtins; the daemon drains it every 1 s | ~4 ms |
| Command Prompt (Clink) | Lua appends successes to the spool; failures go through `reman-hook` for the fix suggestion | no process spawn on success |
| Claude Code hook / fish failures | `reman-hook` (1.9 MB, no ONNX Runtime) | ~Windows process-spawn floor |

## Agents: plug and play

```
reman connect                      status of every agent, plus the folders agents can't see yet
reman connect all                  connect every installed agent
reman connect claude-code codex    connect specific ones
reman connect --add-root D:\proj  let agents see another folder (--remove-root to undo)
reman connect --root D:\proj ...   replace the whole list of folders agents may see
reman connect --old-history on     also share generic commands from old, folder-less history
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

**Old history.** History imported before reman recorded folders (for example, PowerShell history that passed through Atuin) has no folder, so no root can contain it.
- With `--old-history on`, agents also get the *generic* commands from it. A generic command starts with a well-known tool and has no paths, file names, quotes, URLs, hosts, variables or assignments. `docker-compose down` qualifies; `scp app.tar.gz root@host:/root/` does not.
- These results come with `cwd: null` and a `cwd_note`.
- They're never returned when an agent asks about a specific folder.
- It's off by default.

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
