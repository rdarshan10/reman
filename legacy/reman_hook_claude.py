#!/usr/bin/env python3
"""
Reman adapter #3 - Claude Code PostToolUse hook (T7.2).

Fires after each Bash tool run. Extracts {command, exit, cwd, session} from the hook payload
on stdin and PUSHES it to the running Reman daemon (warm model) over the localhost socket,
tagged actor='agent:claude-code'. Thin + stdlib-only: it never loads the embedding model, so it
adds no latency to your commands. Fire-and-forget; if the daemon is down it silently does nothing
(commands just aren't captured that moment). Pure provenance on a real executed command - never
generates anything, and can never break the agent.

Register in Claude Code settings.json:
  "hooks": { "PostToolUse": [ { "matcher": "Bash", "hooks": [ { "type": "command",
    "command": "C:/Users/rdars/reman/.venv/Scripts/python.exe C:/Users/rdars/reman/reman_hook_claude.py" } ] } ] }
"""
import sys, json, os, socket

HOST, PORT = "127.0.0.1", 8765


def main():
    try:
        data = json.load(sys.stdin)
    except Exception:
        return
    if data.get("tool_name") != "Bash":
        return
    cmd = ((data.get("tool_input") or {}).get("command") or "").strip()
    if not cmd:
        return
    cwd = data.get("cwd") or os.getcwd()
    session = data.get("session_id") or os.environ.get("CLAUDE_CODE_SESSION_ID", "")
    event = data.get("hook_event_name", "")
    resp = data.get("tool_response") or {}
    exit_code = 0
    if isinstance(resp, dict):
        for key in ("exit_code", "exitCode", "returncode", "code"):
            if isinstance(resp.get(key), int):
                exit_code = resp[key]; break
        if resp.get("is_error") or resp.get("interrupted"):
            exit_code = exit_code or 1
    # A failed Bash command fires PostToolUseFailure, not PostToolUse. Capture it as a failure
    # so fix-pairs / success-pool / did-you-mean get the failures they depend on.
    if event == "PostToolUseFailure" and exit_code == 0:
        exit_code = 1
    payload = {"op": "ingest", "command": cmd, "exit": exit_code, "cwd": cwd,
               "session": session, "actor": "agent:claude-code"}
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        s.settimeout(1.5)                         # fast; never hang the agent
        s.connect((HOST, PORT))
        s.sendall((json.dumps(payload) + "\n").encode())
        s.recv(256)                               # ack, ignored
        s.close()
    except Exception:
        pass                                      # daemon down -> skip, add no latency


if __name__ == "__main__":
    main()
