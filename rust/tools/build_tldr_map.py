"""Builds tldr_map.json (embedded into reman by describe.rs) from the tldr pages in tldr_pages/.

  python rust/tools/build_tldr_map.py

  cmd: page -> one-line description   (`docker-compose-down` -> "Stop and remove containers, ...")
  ex:  "page\\0sub" -> the description of the page's first example starting with that subcommand;
       "page\\0-b" for one led by a flag (`git checkout -b` -> "Create and switch to a new
       branch"), "page\\0*" for one taking only values (`git checkout main` -> "Switch to an
       existing local branch"), "page\\0/flushdns" for a Windows switch

Rules, each fixing a wrong description seen in real search results:
  * one description per page, by platform: common, then linux, osx, windows, the rest. (Taking
    whichever file came last made `find` "Find a specified string in files", the Windows one.)
    Examples merge across platforms in that order: `ipconfig /flushdns` is only on Windows.
  * alias pages ("This command is an alias of `docker container ls`") take the real page's
    description, with the target named: "List Docker containers (`docker container ls`)".
  * notes are not descriptions: "In PowerShell, this command may be an alias of ...", "See also",
    "Note:", "More information" lines are dropped.
  * EXTRA fills a few frequent subcommands tldr has no page for. It never overrides tldr.
"""
import glob, json, os, re

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
PAGES = os.path.join(ROOT, "tldr_pages")
OUT = os.path.join(ROOT, "tldr_map.json")
ORDER = ["common", "linux", "osx", "windows"]

# reman's own, for frequent subcommands without a tldr page (keys are page names)
EXTRA = {
    "docker-compose-build": "Build or rebuild the images of the services",
    "docker-compose-exec": "Run a command inside a running service container",
    "docker-compose-ps": "List the containers of a Compose project",
    "docker-compose-restart": "Restart the containers of the services",
    "docker-compose-stop": "Stop the running containers of the services without removing them",
    "docker-compose-pull": "Pull the images of the services",
    "docker-compose-run": "Run a one-off command in a new service container",
}

NOT_A_DESCRIPTION = re.compile(r"^(note:|see also|more information|in powershell|some subcommands|also see|part of )", re.I)
ALIAS = re.compile(r"^This command is an alias of `([^`]+)`", re.I)


def clean_example(cmd):
    cmd = re.sub(r"\{\{.*?\}\}", "", cmd)
    cmd = re.sub(r"\[\[.*?\]\]", "", cmd)
    return cmd.strip().strip("`").strip()


def platform_rank(path):
    plat = os.path.basename(os.path.dirname(path))
    return (ORDER.index(plat) if plat in ORDER else len(ORDER), plat, path)


def main():
    gloss, alias, ex = {}, {}, {}
    for path in sorted(glob.glob(os.path.join(PAGES, "**", "*.md"), recursive=True), key=platform_rank):
        stem = os.path.basename(path)[:-3].lower()
        # a higher-precedence platform already described it: only this page's examples count
        described = stem in gloss or stem in alias
        try:
            lines = open(path, encoding="utf-8").read().splitlines()
        except (OSError, UnicodeDecodeError):
            continue
        header, parts, pend = [], [], None
        for ln in lines:
            s = ln.strip()
            if s.startswith("# "):
                header = s[2:].strip().lower().split()
            elif s.startswith("> "):
                g = s[2:].strip().rstrip(".")
                m = ALIAS.match(g)
                if described:
                    pass
                elif m:
                    alias[stem] = m.group(1).strip()
                elif not NOT_A_DESCRIPTION.match(g):
                    parts.append(g)
            elif s.startswith("- ") and s.endswith(":"):
                pend = s[2:-1].strip()
            elif s.startswith("`") and pend:
                toks = clean_example(s).split()
                rest = toks[len(header):] if header and [t.lower() for t in toks[: len(header)]] == header else toks[1:]
                sub = next((t.lower() for t in rest if not t.startswith("-")), None)
                if sub:
                    ex.setdefault(f"{stem}\0{sub}", pend)
                elif rest:
                    ex.setdefault(f"{stem}\0{rest[0].lower()}", pend)
                elif "{{" in s:
                    ex.setdefault(f"{stem}\0*", pend)
                pend = None
        if not described and stem not in alias and parts:
            gloss[stem] = "; ".join(parts)

    # aliases: the real page's words, naming the command it stands for
    for stem, target in alias.items():
        words = target.lower().split()
        real = next((gloss["-".join(words[:n])] for n in range(len(words), 0, -1) if "-".join(words[:n]) in gloss), None)
        if real and stem not in gloss:
            gloss[stem] = f"{real} (`{target}`)"
        elif stem not in gloss:
            gloss[stem] = f"The same as `{target}`"
    for k, v in EXTRA.items():
        gloss.setdefault(k, v)

    json.dump({"cmd": dict(sorted(gloss.items())), "ex": dict(sorted(ex.items()))}, open(OUT, "w", encoding="utf-8"), ensure_ascii=False, separators=(",", ":"))
    print(f"{len(gloss)} pages, {len(ex)} example keys -> {OUT}")
    for k in ("docker-ps", "find", "curl", "rm", "npm-test", "docker-images", "docker-compose-build", "psql", "ipconfig", "bash"):
        print(f"  {k:<22} {gloss.get(k)}")
    for k in ("ipconfig\0/flushdns", "git-checkout\0*", "git-checkout\0-b", "git\0checkout", "psql\0*"):
        print(f"  {k!r:<24} {ex.get(k)}")


if __name__ == "__main__":
    main()
