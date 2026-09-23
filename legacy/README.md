# legacy (Python)

This folder holds the original Python implementation (phases 0–7). It has been superseded by the Rust binary in `../rust` and is no longer wired into anything: the shell profile, the Claude Code hook and the MCP server all point at `~/.reman/bin/reman.exe`.

It is kept for reference and for the parity scripts. Both versions share `~/.reman/reman.db`. The schema changes the Rust binary made are additive (new columns and a `fix_pairs` table), so these scripts can still read the database.
