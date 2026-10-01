# reman

**Your shell history, searchable by meaning, with proof of what worked. For you and your AI agents.**

**Website: [rdarshan10.github.io/reman](https://rdarshan10.github.io/reman)**

One fast Rust binary that remembers every command you and your AI agents run: where it ran, whether it worked, and what fixed it when it didn't. Find anything by describing it ("tear down containers", "run migrations"), not by remembering the exact text.

![Release](https://img.shields.io/github/v/release/rdarshan10/reman) ![Platforms](https://img.shields.io/badge/platforms-Windows%20%7C%20macOS%20%7C%20Linux-blue) ![Rust](https://img.shields.io/badge/built%20with-Rust-orange)

```
   in this folder ─────────────────────────────────────────── │ docker compose down -v
 ▌✓ docker compose down -v                          14×   2d  │ docker: Stop and remove containers, networks,
  ✓ docker compose up -d                            20×   1h  │ images, and volumes created by `docker compose up`
  ✓ docker system prune -af --volumes                3×   1w  │ ✓ worked every time (14 runs) · last 2 days ago
   everywhere ─────────────────────────────────────────────── │ in ~/work/api
  · docker compose logs -f api                      11×   3d  │ matched: close in meaning · shares your words
  · docker images                                    4×   1w  │
   this folder first ←→  ·  by anyone F3  ·  any outcome F2                        24 results
 › tear down containers                                                 Recall  Fixes  Flows
```

## Why

Shell history remembers text, not intent. You know you've done it before, but not the exact flags, the folder, or which of five variants actually worked.

- **Up-arrow and Ctrl+R only match text.** "Tear down containers" never finds `docker-compose down -v`.
- **History forgets outcomes.** A command that failed twice looks the same as one that worked a hundred times.
- **Fixes get lost.** Last month you mistyped a command and then ran the right one. Nothing remembers that pair.
- **AI agents start from zero.** Claude Code, Codex and Cursor guess commands from scratch instead of reusing the ones that already work on your machine.

reman closes that loop: every run is captured with its folder, exit code, duration and who ran it (you or an agent), and all of it is searchable by meaning, in your terminal and by your agents.

## Features

- **Recall by meaning.** Semantic plus fuzzy search over your real history. `run migrations` finds `alembic upgrade head` even though the words don't match. Results from this folder come first, then everywhere; a folder with no good match never dead-ends. When nothing in your history is close, it says so instead of passing off a guess.
- **Proof of what worked.** Every command shows its track record: worked every time, failed every time, or the success rate. A command that only ever failed never leads a list.
- **Fixes.** After a failure, reman prints what worked last time (`gti status` → `git status`), learned from what you actually ran next, with what it changes when it's a variant of what failed (`adds --build`, `ci → install --legacy-peer-deps`). `Alt+F` inserts it.
- **The same error, a different command.** reman keeps what a failed command printed. When a *different* command fails with the same error (`npm ci` hitting the ERESOLVE that `npm install` hit last week), it offers what fixed the first one. Agents get it too by passing the error to `reman_fixes`.
- **A warning before it fails.** Press Enter on a command that failed every time it ran here, or the last 3 times in a row, and reman holds it once when failing costs something: its failures took 10s or more (`failed the last 3 times here, after about 2m each time`), or it installs, builds, migrates or deploys. It says why, shows its last error and what worked instead (`Alt+F` inserts it), and Enter again runs it anyway. A command that fails in a blink just runs. It's an exact lookup on Enter, never while you type (PowerShell, zsh and fish).
- **Flows.** Repeated sequences (`pull → install → test`) are detected. Open one, run its first step, and the next step waits on `↑`.
- **Prediction.** An empty search shows what you usually run next in this folder, and `Alt+N` on an empty prompt puts it there directly (again for the next idea): the next step of a flow you started, the fix for what just failed, then what you usually run after your last command. Only commands you ran yourself, ready to edit, never run.
- **Commands with blanks.** Variants that differ in one place show in the finder as one command with a blank, `git commit -m "‹message›"` or `kubectl logs ‹name› -n prod`, with what went in it lately. Enter puts it on the prompt with the cursor in the blank. Searching for one value (`fix login`) shows that command itself.
- **Time in search.** End a search with when: `deploy last week`, `docker yesterday`, `migrate since monday`, `npm test 3 days ago`. Only what ran then is shown; `yesterday` alone browses that day. Agents' `reman_search` understands it too.
- **How long it takes.** The finder's card says `usually 3m 12s`, and `reman_check` tells agents how long to wait before calling a command hung. A command of yours that ran a minute or more sends a desktop notification when it finishes (`✓ cargo build --release · 3m 04s, faster than usual`); set it to 5 minutes or off in `reman settings`.
- **Your day.** `reman yesterday` (and `reman today`, `reman day monday`) tells what you ran, by project: the commands with repeats folded, failures with what fixed them or that they still fail, and what each agent ran there.
- **Go to a folder by what you did there.** `rcd alembic` goes to the folder where you ran it; `rcd planet api` matches the folder's own name; `rcd` alone lists where you were lately (`rcd 3` goes there).
- **Last time here.** Open a terminal in a folder you left days ago and one line says what you did there: `docker compose up -d → alembic upgrade head → npm run dev`.
- **What broke it?** When a command that kept working here starts failing, reman says how often it worked and what ran in this folder since (`git pull → npm install left-pad`). Every run keeps the git branch and commit it ran on, so it also says when it worked on `main` and fails on `feat/x`, when new commits came in, or when the commit is the same and the change must be elsewhere. `reman why` tells the whole story.
- **Flaky, not broken.** A command whose result here keeps flipping, both ways, with no new commit and no install or pull in between, is called flaky when it fails (`worked 5 of its last 9 runs`), so nobody changes code over it. Agents are told the same.
- **Runbook.** `reman runbook` writes how the project is run, by task (set up, run, test, lint, build, database, deploy), from the commands that actually worked there. It reads through how agents wrap a command (`cd web && npx jest 2>&1 | grep Tests` counts as `npx jest`, run in `web/`), keeps the setup it needs (`. .\msvc-env.ps1; cargo build --release`), says which folder each command runs in when a repo holds several apps, and folds variants (`expo start --clear` / `-c`, `tsc -p tsconfig.json`) into one line. A pipe hides a command's exit status (`npx jest | tail -5` exits with `tail`'s), so the agent hook reads the result the agent saw instead (`Tests: 2 failed`, `test result: ok`, `error TS2345`) and sends only that verdict, never the output; with no verdict, a run counts as run, never as worked. The same reading tells which step of `a && b` failed. Where the history has no whole command for a task (only one test file, say), the one the project declares is shown, marked not run yet: `npm test` from package.json, a Makefile target, `pytest` from its config. With a language model available (a local one is found automatically), it adds a summary, a getting-started order and a note per command; the model only arranges your real commands, never invents one. Agents get the same through `reman_runbook`.
- **Own capture, no Atuin.** Native hooks for PowerShell, bash, zsh and fish (0 to 5 ms per command), plus `r` / `rr` in Command Prompt. What coding agents run is captured too, each tagged with its agent: Claude Code, Codex, Cursor, VS Code (Copilot), Windsurf and Gemini CLI through their own hooks, opencode and pi through a small plugin. Agents that type into your terminal (Copilot, Cursor, Windsurf) are recorded once, with your shell's exact exit code.
- **For AI agents.** A built-in MCP server and a local HTTP endpoint with a folder boundary and secret redaction. `reman connect all` wires Claude Code, Codex, Cursor, VS Code, Windsurf, Gemini CLI and opencode in one step.
- **Agents stuck in a loop.** When Claude Code runs a command that fails the same way for the third time in a session, its hook tells Claude right there: running it again unchanged will fail again, and what fixed that error before, if anything did. `reman_check` says the same about agents' last half hour in a folder. `reman agents` reports what each agent ran this week: runs, failures, and the commands it retried 4+ times while they failed the same way each time.
- **Private by default.** Everything stays in `~/.reman` on your machine. Secrets are masked before anything is saved (`export API_TOKEN=***`), and a command whose secret can't be located isn't saved at all. Commands typed with a leading space are never recorded, nor anything matching your `ignore_commands` / `ignore_folders` patterns; `Del Del` in the finder forgets one everywhere. `reman scrub` masks secrets in history saved before. If you'd rather see your own commands whole, set secrets to *keep as typed* in `reman settings`: they are saved as you typed them, and agents still get them masked.

## Install

### Windows (PowerShell)

```powershell
irm https://raw.githubusercontent.com/rdarshan10/reman/master/install.ps1 | iex
```

Windows PowerShell blocks profile scripts by default on a fresh Windows 10/11 ("running scripts is disabled on this system"), and then reman can't load in new windows. Setup detects this and asks to allow your own scripts for your user only (`Set-ExecutionPolicy RemoteSigned -Scope CurrentUser`, the same step Scoop and oh-my-posh need). `reman doctor` reports it too.

### macOS and Linux

```sh
curl -fsSL https://raw.githubusercontent.com/rdarshan10/reman/master/install.sh | sh
```

Both scripts download the latest release, verify its SHA-256 checksum, and run `reman setup`. Setup is the whole onboarding, with nothing to configure by hand:
1. copies reman into `~/.reman/bin` and starts its background daemon;
2. wires your shells: the PowerShell profile and Command Prompt on Windows, your zsh / bash / fish config on macOS and Linux;
3. connects every coding tool it finds: Claude Code, Claude Desktop, Codex CLI, Cursor, VS Code, Windsurf, Gemini CLI, opencode and pi. Each gets the MCP server (pi has no MCP) and, when it runs shell commands, capture through its own hooks (or a small reman plugin for opencode and pi), so what the agent runs is remembered too. Each config gets a `.reman-bak` backup first.

Everything it set up can be changed later on one page: `reman settings`. To skip connecting coding tools, set `REMAN_NO_CONNECT=1` (or run `reman setup --no-connect`). The first start downloads the embedding model, about 130 MB, once.

### Manual download

Pick your file from the [latest release](https://github.com/rdarshan10/reman/releases/latest):

| platform | file |
|---|---|
| Windows 10/11, x64 | `reman-windows-x64.zip` |
| macOS, Apple Silicon (M1 and later) | `reman-macos-arm64.tar.gz` |
| Linux, x64 | `reman-linux-x64.tar.gz` |

Intel Macs aren't supported: ONNX Runtime, which reman's search uses, publishes no build for them.

Unpack it and run `reman setup` (`reman.exe setup` on Windows). On macOS, a file downloaded through a browser may be quarantined; clear that with `xattr -d com.apple.quarantine reman reman-hook`.

### From source

Requires Rust 1.85 or newer.

```sh
git clone https://github.com/rdarshan10/reman
cd reman/rust
cargo build --release
./target/release/reman setup
```

### Uninstall

```sh
reman uninstall            # lists everything it will remove, then asks
reman uninstall --purge    # also delete your history, settings and the search model
```

It takes reman out of every coding tool (MCP entries, capture hooks, the HTTP endpoint), your PowerShell profile, your zsh / bash / fish config, Command Prompt's `r` / `rr` and your PATH, each by reman's own marker, so everything else in those files stays as it was. Then it stops the daemon and deletes the program. Your history stays in `~/.reman` unless you pass `--purge`. `--dry-run` only lists; `--yes` skips the question.

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
| `F10` inside the finder | settings |

In **Command Prompt**, type `r` (this folder), `r docker` (start with a query), or `rr` (everywhere).

From the command line:

```sh
reman search "tear down containers"   # search by meaning
reman fixes "gti status"              # what worked instead
reman next                            # what you usually run next here
reman check "docker compose up -d"    # has this been run, and did it work?
reman flows --here                    # sequences you repeat in this folder
reman here                            # what you did in this folder last time
reman why                             # it used to work: what ran here since (or: reman why npm test)
reman runbook > RUNBOOK.md            # how this project is run, from what worked
reman scrub                           # mask secrets in older history (dry run; --apply)
reman stats
```

## Settings

```sh
reman settings        # or press F10 inside the finder
```

One page for everything setup configured. Every change is saved as you make it:

- **Coding tools:** connect or disconnect each one with Enter, and turn the local HTTP endpoint (for SDKs and scripts) on or off.
- **What agents can see:** share or unshare folders. Your busiest folders are suggested, with their run counts, and `a` types in any other folder.
- **Privacy:** secrets in commands (mask the value, drop the command, or keep as typed for you while agents still get them masked), strict secret redaction for agents, and whether generic commands from old, folder-less history are shared.
- **Shells:** whether PowerShell, Command Prompt, or your zsh / bash / fish is wired; Enter wires one that isn't.
- **reman:** daemon status, data folder, version.

## Language model (optional)

reman works fully without one. When one is available, `reman runbook` asks it to arrange and explain the project's commands.

- **Found automatically on this machine:** Ollama (`localhost:11434`), LM Studio (`localhost:1234`) or a llama.cpp server (`localhost:8080`). Nothing leaves your machine.
- **Any OpenAI-compatible endpoint, if you set one** in `~/.reman/config.json`. A remote endpoint is only used when set there:

  ```json
  "ai": { "endpoint": "https://api.openai.com/v1", "model": "gpt-4o-mini", "api_key_env": "OPENAI_API_KEY" }
  ```

  The key is read from that environment variable (default `REMAN_AI_KEY`) and never stored.
- **What the model may do:** order, summarise, add a few words per command, and place commands no rule knows into a section. Every command in its answer must be one from your history, character for character, or it is dropped. Commands are redacted before they are sent.
- **Speed:** the answer is cached per project until its commands change. Agents never wait on a model: they get the history-only runbook at once, and the fuller one on their next call.
- **Off:** `reman settings` (Language model), or `reman runbook --static` for one run.

## AI agents

```sh
reman connect all                     # Claude Code, Codex, Cursor, VS Code, Windsurf, Gemini CLI
reman connect                         # status, plus folders agents can't see yet
reman connect --add-root ~/projects/api
reman connect http                    # local HTTP endpoint for the OpenAI Agents SDK, LangChain, curl
```

Agents get eight tools: `reman_search`, `reman_check`, `reman_fixes`, `reman_recent`, `reman_failures`, `reman_flows`, `reman_next` and `reman_runbook` (how the project is run, from what worked there). Every command they get back is one that really ran; reman never generates commands. When nothing close is known, `reman_search` returns nothing rather than the least-bad guess, and says so; the same goes for a result a filter emptied or a folder the agent can't see. A query that names a tool you use (`astro`, `vercel`) is only ever answered with that tool. Commands from the agent's own project come first when matches are close.

What agents can see:
- **Only folders you approve.** Nothing is visible to an agent until you share a folder, not even the project it's working in (a repo, a git worktree of it included; never your whole home folder or a drive).
- **Approve it from the chat.** When an agent needs its project's history, you approve it with your app's own buttons:
  - in apps that support MCP elicitation (VS Code's Copilot, Cursor, Claude Code in a terminal), reman asks you there: *Allow for this session* (nothing is saved; the next session asks again), *Always allow*, or *Don't allow* (not asked again this session);
  - everywhere else (Claude Code in VS Code, Claude Desktop, Gemini CLI...), the agent calls `reman_share_project`, and your app's usual tool approval (*Allow* / *Deny*) decides: Allow shares the project for this session only. After a No, the agent can't ask again that session.

  Either way the decision is yours, not the AI's.
- **Strict permissions** (`reman settings`, under *What agents can see*): only reman's own dialog box can let an agent in: a light or dark card on Windows, a native alert on macOS, zenity or kdialog on Linux. Your AI app's buttons and dialogs no longer count, which matters if you let an app approve tool calls by itself. Where the dialog can't be shown (over SSH), nothing is shared. Only you can turn it off, in your own terminal.
- **Or from your terminal:** `reman connect --add-root <folder>` shares a folder with every agent for good (`--remove-root` undoes it); `reman connect` lists your busiest folders that aren't shared.
- **Secrets are redacted**, and in strict mode (the default) anything still secret-looking is withheld.

Commands your agents run are recorded too, tagged with the agent's name, with how long they took and, when they fail, what they printed: Claude Code (Bash and PowerShell), Codex (approve its hooks once with `/hooks`), Cursor, VS Code's Copilot (and the Copilot CLI), Windsurf and Gemini CLI through their own hooks, opencode and pi through a small reman plugin. Where a tool gives no exit code, or a pipe hides it, the result is read from what the command printed; with nothing clear, the run is recorded with an unknown outcome rather than a guess. Agents that type into your terminal (Copilot, Cursor, Windsurf) are recorded once, with your shell's exact exit code.

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
python e2e/search_quality.py           # search quality, and the website's examples
python e2e/powershell_terminal.py      # real-terminal suites, see rust/e2e/README.md
```
