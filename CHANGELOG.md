# Changelog

## v0.3.2

### Changes you'll notice

- **Key changes in `reman settings` wait for `s`.** Changing a key, turning one off or picking a preset is a draft, marked *not saved*; `s` saves it, `u` undoes it, and leaving with changes unsaved asks first.
- **No new terminal needed.** Open bash, zsh and fish take saved key changes at their next prompt, as PowerShell already did; the finder uses them the next time it opens. Keys reman stops using go back to what they did before. nushell, xonsh and Command Prompt still take them in new terminals.

### New

- `+` on a key gives an action a second key. A refused key keeps the page asking, so the next key you press can be the one.
- The finder's key line shows **F10 settings**, so the settings page can be found from the finder. On a narrow terminal the line shortens its words and leaves out the hints you need least instead of running off the edge.
- Each key has a short name on the page (*Finder, this folder*), with what it does in full on the line below.
- Taking a key your shell moves through history with (Down, PageUp, Ctrl+P, ...) says so: *Down no longer goes forward through history in your shell*.

### Fixed

- PowerShell: a new window printed *Index operation failed; the array index evaluated to null* as reman loaded (v0.3.1). Keys worked regardless.
- PowerShell: the finder on a key other than Up (Down, say) left multi-line input and the predictions list unable to use that key; it now does what it did before there.
- fish: the fix line after a failure names the key that inserts it, as the other shells do.
- The fix line and the next-step message name the key you set, in an open shell too.

### New

- **Your own keys.** Every key reman takes, in your shell and in the finder, can be changed or turned off: `reman settings` (Keys) asks for the new key and says when one is taken, types a character, or is one your shell or terminal needs; `reman keys` lists them, `reman keys set find_all Alt+J` changes one, `reman keys set find_here off` gives a key back. Presets to start from: *standard* (as before, the default), *gentle* (reman leaves ↑, Tab and Enter alone: Ctrl+R, Alt+F and Alt+N only) and *vim*. Every shell takes them: PowerShell, bash, zsh, fish, nushell, xonsh and Command Prompt with Clink. New terminals use them; open PowerShell windows at their next prompt.

## v0.3.0

### Changes you'll notice

- **The finder opens under your prompt**, in the 20 lines below it, instead of taking the whole screen. What you were reading stays in view. `reman settings` (Finder) makes it 30 lines or the whole screen again.
- **The finder lists your own commands at first.** `F3` now goes: yours, yours and agents', agents' only. `reman settings` can list agents' commands from the start.
- **What your agents' commands print is kept** (the end of it, secrets masked, for your newest 3000 runs). `reman settings` (What commands print) keeps less, or none.

### New

- **Every run of a command:** `Ctrl+O` in the finder lists each run with when, how long, the exit code, who, the branch and the folder, and what that run printed. `Del` `Del` there forgets one run.
- **`reman shell`:** your shell inside a terminal layer of reman's own (ConPTY on Windows, a pseudo-terminal on macOS and Linux) that keeps what each of your commands prints, read the way your terminal drew it. Off unless you run it, or turn on *Open shells inside reman shell* in `reman settings`.
- **`reman output`:** what commands printed, found by the command or by the text they printed (`reman output ECONNREFUSED`, `--failed`).
- **`reman sessions` and `reman resume`:** find a coding agent's session by what it did (`reman sessions migration last week`) and reopen it in its folder: Claude Code, Codex, Gemini CLI, opencode.
- **nushell and xonsh:** capture, fixes, the finder, `Alt+N` and `rcd`. `reman setup` wires them when they're installed; `reman uninstall` removes them.
- **Search filters:** `reman search` takes `--failed`, `--worked`, `--by you|agents`, `--after`, `--before`, `--cwd`, `-k`, `--format` and `--json`.
- **`reman delete`** removes runs by their text (never by meaning) after showing them; **`reman prune`** applies ignore rules you added later to older history; **`reman stats week`** (or `month`, `year`, `yesterday`, a date) counts a period.
- **Vim keys** in the finder, as a setting.
- **More secrets masked:** AWS, GitHub, GitLab, Slack, Stripe, npm, Netlify and Pulumi token shapes, wherever they appear in a command.
- **For agents:** a ninth MCP tool, `reman_output`: what a command printed the last times it ran, within the folders you share.

### Fixed

- A search that mentioned a word from a script fed to a command (a heredoc's lines) could be treated as naming a tool, and then find nothing close: `tear down containers` missed `docker-compose down`.
- `reman settings` keeps the note under the selected row in view when the page scrolls.

### Licensing

- reman is now licensed under the Apache License 2.0. Releases include `LICENSE`, `THIRD_PARTY_NOTICES.md` (tldr-pages, Atuin, ONNX Runtime, SQLite, the search model) and `THIRD_PARTY_LICENSES.html` (every Rust library reman is built from, with its license).

Thanks to [Atuin](https://github.com/atuinsh/atuin): its secret patterns and the design of its terminal layer are where `redact.rs` and `reman shell` started.

## v0.2.0

Proof before you run, and memory of how work went: the git branch and commit with every run, holding a command that keeps failing (only when failing costs something), what a fix changes, flaky commands told apart from broken ones, agents' retry loops stopped, `reman agents`, typical run time, commands with a blank, `Alt+N`, done alerts, `rcd` / `reman goto`, `reman yesterday`, and time words in search.

## v0.1.5

The finder's layout holds at every window size.

## v0.1.0 to v0.1.4

The first releases: search by meaning, fixes, flows and prediction, the finder, capture for PowerShell, bash, zsh, fish and Command Prompt, the MCP server and HTTP endpoint for agents, one-step setup and the settings page.
