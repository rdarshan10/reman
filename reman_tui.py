#!/usr/bin/env python3
"""
Reman inline TUI - semantic recall at the prompt, but NOT an Atuin clone.

What makes it different from a fuzzy history finder:
  - SEMANTIC: results are ranked by meaning (warm daemon, ~6ms), not text-match.
  - PROVENANCE PANE: the selected command shows its plain-English description + run count,
    success rate, last-run, and WHO ran it (you vs an AI agent).
  - TWO MODES (Tab): Recall (search) and Fixes (did-you-mean: type a broken command, get the
    real commands from your successes that resemble it).
  - HUMAN-ONLY (Ctrl-U): hide agent-run commands; show only what you personally verified.
  - Actor colour-coding: human-verified vs agent-run at a glance.

Inserts only REAL commands you actually ran - never generates one.

  python reman_tui.py --result-file <path> --query "<initial>"
  python reman_tui.py --selftest "tear down containers"      # headless check
"""
import sys, argparse
import reman_daemon as D

state = {"mode": "recall", "human_only": False, "results": [], "sel": 0}


def _is_agent(r):
    return str(r.get("actor", "")).startswith("agent:")


def fetch(query):
    q = (query or "").strip()
    if not q:
        return []
    op = "search" if state["mode"] == "recall" else "didyoumean"
    try:
        rs = D.client({"op": op, "query": q, "k": 12}).get("results", [])
    except Exception:
        rs = []
    if state["human_only"]:
        rs = [r for r in rs if not _is_agent(r)]
    return rs


def run_tui(initial, result_file):
    from prompt_toolkit import Application
    from prompt_toolkit.buffer import Buffer
    from prompt_toolkit.layout import Layout, HSplit, Window
    from prompt_toolkit.layout.controls import BufferControl, FormattedTextControl
    from prompt_toolkit.key_binding import KeyBindings
    from prompt_toolkit.styles import Style

    qbuf = Buffer()

    def refresh(_=None):
        state["results"] = fetch(qbuf.text)
        state["sel"] = 0
    qbuf.on_text_changed += refresh

    def header():
        m = state["mode"].upper()
        recall = "class:tab_on" if state["mode"] == "recall" else "class:tab_off"
        fixes = "class:tab_on" if state["mode"] == "fixes" else "class:tab_off"
        hu = "class:flag_on" if state["human_only"] else "class:flag_off"
        return [("class:title", " reman "), ("", "  "),
                (recall, " Recall "), ("", " "), (fixes, " Fixes "),
                ("", "    "), (hu, " human-only "), ("", "   "),
                ("class:dim", "Tab: mode · Ctrl-U: human-only")]

    def results_frags():
        rs = state["results"]
        if not rs:
            hint = "type a broken command" if state["mode"] == "fixes" else "type an intent"
            return [("class:dim", f"\n   ({hint} — e.g. 'run database migrations')")]
        out = []
        for i, r in enumerate(rs):
            sel = (i == state["sel"])
            arrow = " ❯ " if sel else "   "
            actor = r.get("actor", "human")
            tag_style = "class:agent" if _is_agent(r) else "class:human"
            tag = "agent" if _is_agent(r) else "you"
            base = "class:sel" if sel else ""
            out.append((base, f"{arrow}"))
            out.append((base, f"{r['command']:<58.58}"))
            out.append((tag_style, f" {tag}\n"))
        return out

    def detail_frags():
        rs = state["results"]
        if not rs:
            return [("class:dim", "")]
        r = rs[state["sel"]]
        desc = r.get("description") or "(no description)"
        sr = r.get("success_rate")
        runs = r.get("run_count", "?")
        last = r.get("last_run", "?")
        actor = r.get("actor", "human")
        line2 = f"runs {runs} · success {int(sr*100) if sr is not None else '?'}% · last {last} · {actor}"
        if state["mode"] == "fixes":
            line2 = (f"typo {r.get('typo','?')} · intent {r.get('intent_sim','?')} · " + line2)
        return [("class:ddesc", f"  {desc}\n"), ("class:dim", f"  {line2}")]

    kb = KeyBindings()

    @kb.add("up")
    def _(e):
        if state["results"]:
            state["sel"] = (state["sel"] - 1) % len(state["results"])

    @kb.add("down")
    def _(e):
        if state["results"]:
            state["sel"] = (state["sel"] + 1) % len(state["results"])

    @kb.add("tab")
    def _(e):
        state["mode"] = "fixes" if state["mode"] == "recall" else "recall"
        refresh()

    @kb.add("c-u")
    def _(e):
        state["human_only"] = not state["human_only"]
        refresh()

    @kb.add("enter")
    def _(e):
        rs = state["results"]
        e.app.exit(result=rs[state["sel"]]["command"] if rs else None)

    @kb.add("c-c")
    @kb.add("escape")
    def _(e):
        e.app.exit(result=None)

    qwin = Window(height=1, content=BufferControl(buffer=qbuf))
    root = HSplit([
        Window(height=1, content=FormattedTextControl(header)),
        Window(height=1, char="─"),
        qwin,
        Window(height=1, char="─"),
        Window(content=FormattedTextControl(results_frags)),
        Window(height=1, char="─"),
        Window(height=3, content=FormattedTextControl(detail_frags)),
    ])
    style = Style.from_dict({
        "title": "reverse bold", "tab_on": "reverse", "tab_off": "#888888",
        "flag_on": "bg:#005f00 #ffffff", "flag_off": "#888888",
        "sel": "reverse", "human": "#5fd75f", "agent": "#ffd75f",
        "dim": "#888888", "ddesc": "#d7d7af", "prompt": "bold",
    })
    app = Application(layout=Layout(root, focused_element=qwin), key_bindings=kb,
                      style=style, full_screen=True)

    if initial:
        qbuf.text = initial
        refresh()
    chosen = app.run()
    if chosen:
        if result_file:
            open(result_file, "w", encoding="utf-8").write(chosen)
        else:
            print(chosen)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--result-file")
    ap.add_argument("--query", default="")
    ap.add_argument("--selftest")
    args = ap.parse_args()
    if args.selftest is not None:
        for mode in ("recall", "fixes"):
            state["mode"] = mode
            rs = fetch(args.selftest)
            print(f"[{mode}] {args.selftest!r} -> {len(rs)} results")
            for r in rs[:3]:
                print(f"   {r['command']}   actor={r.get('actor')} success={r.get('success_rate')} desc={(r.get('description') or '')[:40]!r}")
        return
    run_tui(args.query, args.result_file)


if __name__ == "__main__":
    main()
