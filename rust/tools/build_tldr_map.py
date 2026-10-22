"""Builds tldr_map.json (embedded into reman by describe.rs) from the tldr pages in tldr_pages/.

  python rust/tools/build_tldr_map.py

  cmd: page -> one-line description   (`docker-compose-down` -> "Stop and remove containers, ...")
  ex:  page -> its examples, in order: [description, literal words, takes a value]. The literal
       words are what a command must contain for the example to describe it: subcommands,
       keywords and options, alternatives joined by `|` (`{{[-d|--delete]}}` -> "-d|--delete");
       values (`{{path/to/file}}`) are not words, only "takes a value". describe.rs picks the
       example whose words a command has: `New-Item -ItemType Directory x` -> "Create a new
       directory", `git branch -D x` -> "Delete a local branch", `git checkout main` (a value
       only) -> "Switch to an existing local branch"

Rules, each fixing a wrong description seen in real search results:
  * one description per page, by platform: common, then linux, osx, windows, the rest. (Taking
    whichever file came last made `find` "Find a specified string in files", the Windows one.)
    Examples merge across platforms in that order: `ipconfig /flushdns` is only on Windows.
  * alias pages ("This command is an alias of `docker container ls`") take the real page's
    description, with the target named: "List Docker containers (`docker container ls`)".
  * notes are not descriptions: "In PowerShell, this command may be an alias of ...", "See also",
    "Note:", "More information" lines are dropped.
  * EXTRA fills a few frequent subcommands tldr has no page for. It never overrides tldr.
  * options written `{{[-v|--version]}}` (most pages since 2024) are words, not values: dropping
    them as placeholders lost every such example (`git branch {{[-d|--delete]}}`).
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
    # PowerShell cmdlets people run every day that tldr has no page for
    "copy-item": "Copy files and folders to another location",
    "get-process": "List the processes running on this computer",
    "stop-process": "Stop (kill) running processes by ID or name",
    "get-nettcpconnection": "List TCP connections and the ports in use, such as which process is listening on a port",
    "get-service": "List the services on this computer and whether they are running",
    "set-executionpolicy": "Set the PowerShell execution policy, which decides what scripts may run",
}

NOT_A_DESCRIPTION = re.compile(r"^(note:|see also|more information|in powershell|some subcommands|also see|part of )", re.I)
ALIAS = re.compile(r"^This command is an alias of `([^`]+)`", re.I)


OPTIONS = re.compile(r"\{\{\[(.*?)\]\}\}")
PLACEHOLDER = re.compile(r"\{\{(.*?)\}\}")
WORD = re.compile(r"^[a-z][a-z0-9_.:-]*$")
VALUE = "\x01"


def alternatives(text):
    """`-d|--delete` -> "-d|--delete"; an alternative of several words counts by its first."""
    alts = [a.split()[0].lower() for a in text.split("|") if a.split()]
    return "|".join(alts)


def example_words(cmd):
    """The literal words of an example command line, and whether it takes a value."""
    cmd = cmd.strip().strip("`").strip()
    cmd = OPTIONS.sub(lambda m: f" {alternatives(m.group(1))} ", cmd)

    def placeholder(m):
        alts = m.group(1).split("|")
        # a choice of keywords (`{{SymbolicLink|HardLink|Junction}}`) is words; anything else a value
        if len(alts) > 1 and all(WORD.match(a.strip().lower()) for a in alts):
            return f" {alternatives(m.group(1))} "
        return f" {VALUE} "

    cmd = PLACEHOLDER.sub(placeholder, cmd)
    toks = [t.strip("\"'") for t in cmd.split()]
    words, value = [], False
    for t in toks:
        low = t.lower()
        if t == VALUE:
            value = True
        elif low in ("|", "||", "&&", ">", ">>", "<", "&"):
            continue
        elif (low.startswith(("-", "/")) and len(low) > 1) or WORD.match(low) or ("|" in low and low.strip("|")):
            words.append(low)
        else:
            value = True  # a literal file name, a quoted string: a value of the example's own
    return words, value


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
                words, value = example_words(s)
                if words[:1] == ["sudo"]:
                    words = words[1:]
                # the page's own command words (`git branch`) aren't the example's
                # (`npm {{[i|install]}}` on the npm-install page: an alternative is the word too)
                own = header and len(words) >= len(header) and all(h in w.split("|") for h, w in zip(header, words))
                # an example of another command (venv's `deactivate`) says nothing about this one
                if own:
                    ex.setdefault(stem, []).append([pend, words[len(header):], int(value)])
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
    for k in ("ipconfig", "git-checkout", "git-branch", "new-item"):
        print(f"  {k:<14} {ex.get(k, [])[:4]}")


if __name__ == "__main__":
    main()
