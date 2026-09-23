#!/usr/bin/env python3
"""
Reman adapter #2 - history-file adapter (bash), per spec 0.5 tier-2.

Proves ingestion is NOT PowerShell-only: reads a real ~/.bash_history (plain commands, one per
line - no exit/cwd/time, the tier-2 provenance limitation) and pushes each to the warm daemon
tagged actor='human'. Dedups against the existing corpus by the daemon's hash. Pure retrieval
fodder - real commands the user actually ran in bash.

  python reman_adapter_bash.py [path]      # default ~/.bash_history
"""
import sys, os, socket, json

HOST, PORT = "127.0.0.1", 8765


def push(payload):
    try:
        s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        s.settimeout(3); s.connect((HOST, PORT))
        s.sendall((json.dumps(payload) + "\n").encode())
        ack = s.recv(256); s.close()
        return json.loads(ack.decode()).get("ok", False)
    except Exception as e:
        print(f"daemon push failed ({e}) — is the daemon running?", file=sys.stderr)
        return None


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else os.path.expanduser("~/.bash_history")
    if not os.path.exists(path):
        print(f"no bash history at {path}"); return
    seen, sent, failed = set(), 0, 0
    for line in open(path, encoding="utf-8", errors="replace"):
        cmd = line.strip()
        if not cmd or cmd in seen:
            continue
        seen.add(cmd)
        ok = push({"op": "ingest", "command": cmd, "exit": None, "cwd": None,
                   "session": "bash-history-import", "actor": "human"})
        if ok is None:
            print("aborting: daemon unreachable"); return
        sent += 1 if ok else 0
        failed += 0 if ok else 1
    print(f"bash adapter: pushed {sent} unique commands from {path} (tagged human), {failed} failed",
          file=sys.stderr)


if __name__ == "__main__":
    main()
