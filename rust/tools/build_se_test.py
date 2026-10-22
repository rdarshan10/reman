"""A test set of how people really ask for commands: Stack Exchange question titles ("How do I
delete a local git branch?") paired with the command their best answer gives. For measuring the
embedding model only (embed_bench.rs); never trained on, never shipped (it is CC-BY-SA).

  python rust/tools/build_se_test.py SE_DIR OUT.jsonl [--n 3000]

SE_DIR: *.jsonl.gz of [title, best answer] (flax-sentence-embeddings/
stackexchange_title_best_voted_answer_jsonl). A pair is kept when the title asks how to do
something and the answer has a line that is a command of a tool tldr knows.
"""
import argparse, glob, gzip, html, json, os, random, re

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
ASKS = re.compile(r"^(how (do|can|to|would|should) |how do you |is there a way to )", re.I)
PROMPT = re.compile(r"^(\$|#|>|PS [^>]*>)\s+")


def tools():
    m = json.load(open(os.path.join(ROOT, "tldr_map.json"), encoding="utf-8"))
    return {k for k in m["cmd"] if "-" not in k} | {k.split("-")[0] for k in m["cmd"]}


PROSE = {"the", "your", "you", "is", "it", "this", "that", "will", "should", "can", "just", "then", "which", "and", "are", "be", "for", "of"}
SHELL = re.compile(r"(^|\s)(-{1,2}\w|/|~|\||>|\w=)")


def command_in(answer, known):
    """The first line of the answer that is a command: typed in lowercase, a tool tldr knows, an
    argument that looks like a shell's (a flag, a path, a pipe, a redirect, an assignment), and
    not prose."""
    for line in html.unescape(answer).splitlines():
        s = PROMPT.sub("", line.strip())
        if not (4 <= len(s) <= 120) or s.endswith((".", ":", "?", ",")) or s.count(" ") > 14:
            continue
        toks = s.split()
        first = toks[1] if toks[0] == "sudo" and len(toks) > 1 else toks[0]
        if first != first.lower() or first not in known or len(toks) < 2:
            continue
        rest = " ".join(toks[1:])
        unquoted = re.sub(r"(['\"]).*?\1", "", rest)
        if not SHELL.search(rest) or sum(w in PROSE for w in unquoted.lower().split()) >= 2:
            continue
        return s
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("se_dir")
    ap.add_argument("out")
    ap.add_argument("--n", type=int, default=3000)
    a = ap.parse_args()
    known = tools() - {"a", "i", "if", "for", "the", "to", "in", "is", "it", "on", "and", "or", "you", "this", "then", "do", "try", "use", "run", "see", "set", "with"}
    rows, seen = [], set()
    for path in sorted(glob.glob(os.path.join(a.se_dir, "*.jsonl.gz"))):
        if os.path.basename(path).startswith("meta."):
            continue
        site = os.path.basename(path).split(".jsonl")[0]
        try:
            f = gzip.open(path, "rt", encoding="utf-8")
            f.peek(1) if hasattr(f, "peek") else None
        except OSError as e:
            print(f"skipping {site}: {e}")
            continue
        with f:
            for line in f:
                title, answer = json.loads(line)
                title = html.unescape(title).strip()
                if not ASKS.match(title):
                    continue
                cmd = command_in(answer, known)
                if cmd and cmd not in seen:
                    seen.add(cmd)
                    first = cmd.split()[1] if cmd.startswith("sudo ") else cmd.split()[0]
                    rows.append({"tool": first.lower(), "desc": title, "cmd": cmd, "site": site})
    random.Random(7).shuffle(rows)
    rows = rows[: a.n]
    with open(a.out, "w", encoding="utf-8") as f:
        for r in rows:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")
    print(f"{len(seen)} usable pairs; wrote {len(rows)} -> {a.out}")


if __name__ == "__main__":
    main()
