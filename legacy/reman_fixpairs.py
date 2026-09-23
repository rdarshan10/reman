#!/usr/bin/env python3
"""
Reman Phase 4 - Fix-pair detection. Store a failure ONLY when a real fix follows; unpaired
failures evaporate. Failures never go in `commands` - only here, once a fix is proven.

Certainty ladder (highest wins; lower tiers only if higher don't apply):
  1. exact            - same cmd_text failed earlier, later succeeded (env/dep/file fixed)
  2. shell_correction - fix is an edited rerun of the immediately-preceding failed line (typo)
  3. inferred         - same session, fix within N commands, same base program
  else                - discard the buffered failure (transient noise)

R3 CAVEAT: Atuin records exit codes, NOT stderr. So `error_tail` has no data source and is
left None here (detection-only, per spec R3 option a). Detection itself survives on exit-flips
+ edit-distance + same-session similarity. session id is available ({session}, R2 confirmed).
"""
import sys

WINDOW = 10   # 'inferred' fix must be within this many commands of the failure (same session)


def _lev(a, b):
    # Damerau-Levenshtein (OSA): an adjacent transposition costs 1, not 2 - real typos are
    # often transpositions (gti->git), which plain Levenshtein over-penalises.
    if a == b:
        return 0
    la, lb = len(a), len(b)
    if not la:
        return lb
    if not lb:
        return la
    d = [[0] * (lb + 1) for _ in range(la + 1)]
    for i in range(la + 1):
        d[i][0] = i
    for j in range(lb + 1):
        d[0][j] = j
    for i in range(1, la + 1):
        for j in range(1, lb + 1):
            cost = 0 if a[i - 1] == b[j - 1] else 1
            d[i][j] = min(d[i - 1][j] + 1, d[i][j - 1] + 1, d[i - 1][j - 1] + cost)
            if i > 1 and j > 1 and a[i - 1] == b[j - 2] and a[i - 2] == b[j - 1]:
                d[i][j] = min(d[i][j], d[i - 2][j - 2] + 1)   # adjacent transposition
    return d[la][lb]


def norm_edit(a, b):
    return _lev(a, b) / max(len(a), len(b), 1)


def base(cmd):
    toks = cmd.split()
    if not toks:
        return ""
    if len(toks) > 1 and not toks[1].startswith("-"):
        return f"{toks[0]} {toks[1]}"
    return toks[0]


def classify(failed, fixed, gap):
    """gap = how many commands later the fix ran (1 = immediately after)."""
    if failed == fixed:
        return "exact"
    if gap == 1 and norm_edit(failed, fixed) <= 0.15:   # near-identical = typo rerun (0.056),
        return "shell_correction"                       # not a subcommand swap (0.20). tunable, spec 6
    if gap <= WINDOW and base(failed) == base(fixed) and failed != fixed:
        return "inferred"
    return None


def detect_in_session(events):
    """events: ordered list of dicts {pos, cmd, exit}. Returns list of fix_pair dicts."""
    buffer, pairs = [], []
    for ev in events:
        if ev["exit"] != 0:
            buffer.append(ev)                 # buffer the failure
            continue
        for f in reversed(buffer):            # success: resolve buffered failures, newest first
            conf = classify(f["cmd"], ev["cmd"], ev["pos"] - f["pos"])
            if conf:
                pairs.append({"failed_text": f["cmd"], "fixed_cmd": ev["cmd"],
                              "confidence": conf, "error_tail": None})   # R3: no stderr -> None
                buffer.remove(f)
                break
    # session close: unpaired failures evaporate (never stored)
    return pairs


def detect(events_by_session):
    out = []
    for sess, events in events_by_session.items():
        out.extend(detect_in_session(sorted(events, key=lambda e: e["pos"])))
    return out


def simulate():
    # one session, a realistic fail->fix stream + an unpaired failure that must evaporate
    S = [
        {"pos": 1, "cmd": "pip install nummpy",            "exit": 1},  # typo
        {"pos": 2, "cmd": "pip install numpy",             "exit": 0},  # -> shell_correction (adjacent, close edit)
        {"pos": 3, "cmd": "python app.py",                 "exit": 1},  # missing dep
        {"pos": 4, "cmd": "pip install -r requirements.txt","exit": 0},
        {"pos": 5, "cmd": "python app.py",                 "exit": 0},  # -> exact (same cmd failed@3 then ok@5)
        {"pos": 6, "cmd": "docker compose pull",           "exit": 1},  # fails
        {"pos": 7, "cmd": "docker compose up -d",          "exit": 0},  # -> inferred (same base 'docker compose', within window)
        {"pos": 8, "cmd": "terraform apply",              "exit": 1},   # UNPAIRED failure -> must evaporate
    ]
    pairs = detect_in_session(S)
    got = {(p["failed_text"], p["fixed_cmd"]): p["confidence"] for p in pairs}
    expect = {
        ("pip install nummpy", "pip install numpy"): "shell_correction",
        ("python app.py", "python app.py"): "exact",
        ("docker compose pull", "docker compose up -d"): "inferred",
    }
    unpaired_gone = all(p["failed_text"] != "terraform apply" for p in pairs)
    all_tail_none = all(p["error_tail"] is None for p in pairs)
    ok = got == expect and unpaired_gone and all_tail_none

    print("detected fix pairs:")
    for p in pairs:
        print(f"  [{p['confidence']:<16}] {p['failed_text']!r} -> {p['fixed_cmd']!r}  (error_tail={p['error_tail']})")
    print(f"\nunpaired 'terraform apply' failure evaporated: {unpaired_gone}")
    print(f"error_tail None for all (R3 detection-only): {all_tail_none}")
    print(f"\nAC {'PASS' if ok else 'FAIL'}: each tier detected correctly; unpaired failure left nothing.")
    return ok


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "simulate":
        sys.exit(0 if simulate() else 1)
    else:
        print("usage: reman_fixpairs.py simulate    (runs the Phase 4 detection AC)")
