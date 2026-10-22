"""(description, command) pairs from the tldr pages, split by tool, for training and measuring the
embedding model on commands (see train_embedder.py).

  python rust/tools/build_cmd_pairs.py OUT_DIR

Writes OUT_DIR/train.jsonl and OUT_DIR/test.jsonl, one {"tool", "page", "desc", "cmd"} per line.
A tool's pages all land on one side (by a hash of the tool's name: the same split every run), so
the test side is tools the model never saw: what reman meets in your own scripts and in tools tldr
has no page for. Placeholders become plain values (`{{path/to/file}}` -> `path/to/file`), options
their first spelling (`{{[-d|--delete]}}` -> `-d`).
"""
import glob, hashlib, json, os, re, sys

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
PAGES = os.path.join(ROOT, "tldr_pages")
ORDER = ["common", "linux", "osx", "windows"]
TEST_SHARE = 15  # percent of tools held out

OPTIONS = re.compile(r"\{\{\[(.*?)\]\}\}")
PLACEHOLDER = re.compile(r"\{\{(.*?)\}\}")


def concrete(example):
    cmd = example.strip().strip("`").strip()
    cmd = OPTIONS.sub(lambda m: m.group(1).split("|")[0].strip(), cmd)
    cmd = PLACEHOLDER.sub(lambda m: m.group(1).split("|")[0].strip(), cmd)
    return re.sub(r"\s+", " ", cmd).strip()


def held_out(tool):
    return int(hashlib.sha256(tool.encode()).hexdigest(), 16) % 100 < TEST_SHARE


def main(out):
    os.makedirs(out, exist_ok=True)
    seen, rows = set(), []
    paths = sorted(glob.glob(os.path.join(PAGES, "**", "*.md"), recursive=True),
                   key=lambda p: (ORDER.index(os.path.basename(os.path.dirname(p))) if os.path.basename(os.path.dirname(p)) in ORDER else 9, p))
    for path in paths:
        page = os.path.basename(path)[:-3].lower()
        if not re.match(r"^[a-z0-9]", page):
            continue  # `!`, `$`... shell symbols
        tool = page.split("-")[0]
        desc = None
        for ln in open(path, encoding="utf-8", errors="ignore").read().splitlines():
            s = ln.strip()
            if s.startswith("- ") and s.endswith(":"):
                desc = s[2:-1].strip()
            elif s.startswith("`") and desc:
                cmd = concrete(s)
                key = (desc.lower(), cmd.lower())
                if cmd and key not in seen:
                    seen.add(key)
                    rows.append({"tool": tool, "page": page, "desc": desc, "cmd": cmd})
                desc = None
    train = [r for r in rows if not held_out(r["tool"])]
    test = [r for r in rows if held_out(r["tool"])]
    for name, part in (("train", train), ("test", test)):
        with open(os.path.join(out, f"{name}.jsonl"), "w", encoding="utf-8") as f:
            for r in part:
                f.write(json.dumps(r, ensure_ascii=False) + "\n")
    tools = lambda part: len({r["tool"] for r in part})
    print(f"{len(rows)} pairs: train {len(train)} ({tools(train)} tools), test {len(test)} ({tools(test)} tools) -> {out}")


if __name__ == "__main__":
    main(sys.argv[1])
