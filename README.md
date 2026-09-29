# reman

**Your shell history, searchable by meaning, with proof of what worked. For you and your AI agents.**

One fast Rust binary that remembers every command you and your AI agents run: where it ran, whether it worked, and what fixed it when it didn't. Find anything by describing it ("tear down containers", "run migrations"), not by remembering the exact text.

![Release](https://img.shields.io/github/v/release/rdarshan10/reman) ![Platforms](https://img.shields.io/badge/platforms-Windows%20%7C%20macOS%20%7C%20Linux-blue) ![Rust](https://img.shields.io/badge/built%20with-Rust-orange)

```
   in this folder ─────────────────────────────────────────── │ docker compose exec web alembic upgrade head
 ▌✓ docker compose exec web alembic upgrade head   18×   6d  │ alembic: Upgrade the database to the latest
  ✓ docker compose exec web alembic current          2×  2mo  │ revision
  ~ docker-compose up --build -d                   488×   6d  │ ✓ worked every time (18 runs) · last 6 days ago
   everywhere ─────────────────────────────────────────────── │ in D:\PlanetNaidu\planet_naidu_api
  · alembic upgrade head                           324×  3mo  │ matched: close in meaning
  · docker exec planet_naidu_api_web alembic upg…  132×  3mo  │
   this folder first ←→  ·  by anyone F3  ·  any outcome F2                        31 results
 › run migrations                                                       Recall  Fixes  Flows
```

## Why

Shell history remembers text, not intent. You know you've done it before, but not the exact flags, the folder, or which of five variants actually worked.

- **Up-arrow and Ctrl+R only match text.** "Tear down containers" never finds `docker-compose down -v`.
- **History forgets outcomes.** A command that failed twice looks the same as one that worked a hundred times.
- **Fixes get lost.** Last month you mistyped a command and then ran the right one. Nothing remembers that pair.
- **AI agents start from zero.** Claude Code, Codex and Cursor guess commands from scratch instead of reusing the ones that already work on your machine.

reman closes that loop: every run is captured with its folder, exit code, duration and who ran it (you or an agent), and all of it is searchable by meaning, in your terminal and by your agents.

## Features

- **Recall by meaning.** Semantic plus fuzzy search over your real history. `run migrations` finds `alembic upgrade head` even though the words don't match. Results from this folder come first, then everywhere; a folder with no good match never dead-ends.
- **Proof of what worked.** Every command shows its track record: worked every time, failed every time, or the success rate. A command that only ever failed never leads a list.
- **Fixes.** After a failure, reman prints what worked last time (`gti status` → `git status`), learned from what you actually ran next. `Alt+F` inserts it.
- **Flows.** Repeated sequences (`pull → install → test`) are detected. Open one, run its first step, and the next step waits on `↑`.
- **Prediction.** An empty search shows what you usually run next in this folder.
- **Own capture, no Atuin.** Native hooks for PowerShell, bash, zsh and fish (0 to 5 ms per command), plus `r` / `rr` in Command Prompt.
- **For AI agents.** A built-in MCP server and a local HTTP endpoint with a folder boundary and secret redaction. `reman connect all` wires Claude Code, Codex, Cursor, VS Code, Windsurf and Gemini CLI in one step.
- **Private by default.** Everything stays in `~/.reman` on your machine. Commands typed with a leading space are never recorded; `Del Del` in the finder forgets one everywhere.

## Install

### Windows (PowerShell)

```powershell
irm https://raw.githubusercontent.com/rdarshan10/reman/master/install.ps1 | iex
```

### macOS and Linux

```sh
curl -fsSL https://raw.githubusercontent.com/rdarshan10/reman/master/install.sh | sh
```

Both scripts download the latest release, verify its SHA-256 checksum, and run `reman setup`. That copies reman into `~/.reman/bin`, starts its background daemon, and wires your shell (PowerShell profile, Command Prompt macros, or your zsh / bash / fish config). The first start downloads the embedding model, about 130 MB, once.

### Manual download

Pick your file from the [latest release](https://github.com/rdarshan10/reman/releases/latest):

| platform | file |
|---|---|
| Windows 10/11, x64 | `reman-windows-x64.zip` |
| macOS, Apple Silicon | `reman-macos-arm64.tar.gz` |
| macOS, Intel | `reman-macos-x64.tar.gz` |
| Linux, x64 | `reman-linux-x64.tar.gz` |

Unpack it and run `reman setup` (`reman.exe setup` on Windows). On macOS, a file downloaded through a browser may be quarantined; clear that with `xattr -d com.apple.quarantine reman reman-hook`.

### From source

Requires Rust 1.85 or newer.

```sh
git clone https://github.com/rdarshan10/reman
cd reman/rust
cargo build --release
./target/release/reman setup
```

## Getting started

Open a new terminal after installing.

| you press / type | what happens |
|---|---|
| `↑` | the finder, for this folder |
| `Ctrl+R` | the finder, for all folders |
| `Tab` / `Shift+Tab` inside the finder | switch between **Recall**, **Fixes** and **Flows** |
| `Enter` | put the command on your prompt (it never runs by itself) |
| `Alt+F` | insert the fix suggested after a failed command |
| `F1` inside the finder | every key |

In **Command Prompt**, type `r` (this folder), `r docker` (start with a query), or `rr` (everywhere).

From the command line:

```sh
reman search "tear down containers"   # search by meaning
reman fixes "gti status"              # what worked instead
reman next                            # what you usually run next here
reman check "docker compose up -d"    # has this been run, and did it work?
reman flows --here                    # sequences you repeat in this folder
reman stats
```

## AI agents

```sh
reman connect all                     # Claude Code, Codex, Cursor, VS Code, Windsurf, Gemini CLI
reman connect                         # status, plus folders agents can't see yet
reman connect --add-root ~/projects/api
reman connect http                    # local HTTP endpoint for the OpenAI Agents SDK, LangChain, curl
```

Agents get seven tools: `reman_search`, `reman_check`, `reman_fixes`, `reman_recent`, `reman_failures`, `reman_flows` and `reman_next`. They only see commands from the folders you share, secrets are redacted, and every command they get back is one that really ran; reman never generates commands.

## Performance

Measured on a history of about 6,000 commands and 21,000 runs:

| operation | p50 |
|---|---|
| search by meaning | 7 to 9 ms |
| browse recent | 3.5 ms |
| did-you-mean | 4 ms |
| next-command prediction | 0.2 ms |
| capture, PowerShell | no process spawned |
| capture, bash / zsh / fish | 5 / 2 / 5 ms |

## Architecture

```
rust/src/
├── daemon.rs      warm daemon: model loaded once, whole history in memory, JSON lines on localhost
├── store.rs       in-memory model of the history (vectors, provenance, per-folder stats)
├── search.rs      hybrid ranking: meaning (bge-small) + fuzzy text + track record
├── dym.rs         did-you-mean for failed commands
├── fixpairs.rs    learned fixes: what you ran after a failure, and it worked
├── flows.rs       repeated command sequences
├── predict.rs     next-command prediction
├── tui.rs         the finder
├── complete.rs    Tab completion for reman itself, one engine for every shell
├── mcp.rs         MCP server for agents, with the folder boundary
├── http.rs        local HTTP endpoint (MCP Streamable HTTP + REST)
├── connect.rs     `reman connect`: wires each agent's config
├── capture.rs     recording, used by the shell hooks and reman-hook
└── init/          shell integrations: PowerShell, bash, zsh, fish, Command Prompt
```

Detailed documentation, every key and the storage layout: [rust/README.md](rust/README.md). The real-terminal and agent test suites: [rust/e2e](rust/e2e).

## Roadmap

- ✅ Capture everywhere: PowerShell, bash, zsh, fish, Command Prompt, Claude Code
- ✅ Hybrid search, fixes, flows, prediction
- ✅ The finder: sections, track record, flows you can walk through
- ✅ Agents: MCP server, HTTP endpoint, one-command connectors
- 🚧 Signed binaries for Windows and macOS
- 🚧 Homebrew tap and winget package
- 🚧 Sync between machines

## Contributing

Issues and pull requests are welcome, especially new shell integrations, agent connectors and search-quality examples. Please open an issue first for larger changes.

To run the tests:

```sh
cd rust
cargo test
python e2e/powershell_terminal.py      # real-terminal suites, see rust/e2e/README.md
```
