# reman (Rust)

Semantic, provenance-aware shell history for you and your AI agents. It is one binary with no Atuin and no Python.

## Install / cut over

```powershell
cargo build --release
.\target\release\reman.exe setup
```

`setup` runs these steps:
1. Copies the binary to `~/.reman/bin`, so a running daemon never locks your build output.
2. Starts the warm daemon.
3. Imports Atuin history one last time.
4. Removes the double captures that Atuin and the prompt hook produced together.
5. Backfills fix-pairs.
6. Rewires your PowerShell profile. The atuin and legacy Python blocks are removed, and a backup is kept as `*.reman-bak`.

For bash, zsh and fish, add this to your rc file instead: `eval "$(reman init zsh)"`.

## Keys (PowerShell, bash, zsh, fish)

| key | action |
|---|---|
| `UpArrow` | open the finder, scoped to this folder |
| `Ctrl+R` | open the finder across all folders |
| `Alt+F` (pwsh) | insert the fix suggested after a failed command |

The finder has three tabs: **Recall**, **Fixes** and **Flows**. `Ctrl+T` switches between them.

| finder key | action |
|---|---|
| `←` / `→` | scope: folder → repo → all |
| `Tab` | actor: all → you → agent |
| `F2` | pass/fail filter |
| `Ctrl+G` | group variants |
| `Ctrl+P` | pin |
| `Del` `Del` | forget the command everywhere |
| `Enter` | insert the command |
| `Esc` | close |

With an empty query, the finder shows **predicted next commands** first.

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
reman mcp                    MCP stdio server (same tools as before + reman_next)
reman hook claude            Claude Code PostToolUse/PostToolUseFailure hook
```

### Agents

Register the MCP server and the Claude Code hook:

- MCP: `claude mcp add reman -- ~/.reman/bin/reman.exe mcp`, with `REMAN_MCP_ROOT=<dir1>;<dir2>` as the security boundary. `REMAN_MCP_STRICT_SECRETS=1` drops commands that still look secret after redaction.
- Hook (`settings.json`): `"command": "C:/Users/<you>/.reman/bin/reman.exe hook claude"`.

## Storage

Everything lives in `~/.reman/`:

| path | what |
|---|---|
| `reman.db` | SQLite, same schema as the Python version |
| `spool.jsonl` | runs captured while the daemon was down, drained automatically |
| `models/` | bge-small-en-v1.5 (quantized ONNX) |
| `daemon.log` | daemon log |

To keep a command out of history, start it with a leading space, or match it with the `REMAN_IGNORE=<regex>` environment variable.
