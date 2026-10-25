"""PowerShell's cmdlets from Microsoft's documentation (MicrosoftDocs/PowerShell-Docs and
windows-powershell-docs; docs CC BY 4.0, example code MIT): each cmdlet's synopsis and its
examples' titles and commands, for

  * the description map (build_tldr_map.py --psdocs): `Get-CimInstance` described like any
    tldr page, for every user, at no cost at search time;
  * training (train_embedder.py): (description, command) pairs, split by cmdlet like
    build_cmd_pairs.py splits tools, so the test side is cmdlets the model never saw.

  python rust/tools/build_ps_pairs.py DOCS_ROOT... --out DATA_DIR

DOCS_ROOT: an unpacked repository (PowerShell-Docs-main or windows-powershell-docs-main).
Writes DATA_DIR/ps_train.jsonl, ps_test.jsonl and ps_pages.json.
"""
import argparse, glob, json, os, re, sys

sys.path.insert(0, os.path.dirname(__file__))
from build_cmd_pairs import held_out  # the same split as tldr's

# where the cmdlets are, newest docs first (a cmdlet documented twice keeps the first)
LAYOUTS = ["reference/7.5/*/*.md", "reference/5.1/*/*.md", "docset/winserver2025-ps/*/*.md"]
# Windows modules a developer's machine uses; the rest (Active Directory, ADFS, DHCP server, ...)
# go in the description map but not into paraphrasing
CLIENT_MODULES = {
    "NetTCPIP", "NetAdapter", "NetConnection", "NetSecurity", "DnsClient", "SmbShare", "Storage", "ScheduledTasks",
    "Appx", "Defender", "BitLocker", "PrintManagement", "International", "PKI", "Dism", "Hyper-V", "NetQos",
    "WindowsUpdate", "PnpDevice", "TrustedPlatformModule", "SecureBoot", "Microsoft.PowerShell.LocalAccounts",
    "NetLbfo", "VpnClient", "Wdac", "WindowsErrorReporting", "ConfigCI", "StartLayout",
}
NAME = re.compile(r"^[A-Z][a-z]+-[A-Za-z0-9]+$")
PROMPT = re.compile(r"^PS [^>]*>\s*")


def section(text, title):
    m = re.search(rf"^## {title}\s*$(.*?)(?=^## |\Z)", text, re.M | re.S)
    return m.group(1).strip() if m else ""


def first_sentence(text):
    text = " ".join(text.split())
    m = re.match(r"(.+?[.!?])(\s|$)", text)
    return (m.group(1) if m else text).rstrip(".")


def examples(text, name):
    """(title, command) for each example whose code runs the cmdlet."""
    out = []
    for m in re.finditer(r"^### Example \d+\s*[:-]?\s*(.*?)\s*$(.*?)(?=^### |^## |\Z)", text, re.M | re.S):
        title, body = m.group(1).strip().rstrip("."), m.group(2)
        code = re.search(r"```(?:powershell|PowerShell|ps)?\s*\n(.*?)```", body, re.S)
        if not title or not code:
            continue
        for line in code.group(1).splitlines():
            line = PROMPT.sub("", line.strip())
            if name.lower() in line.lower() and not line.startswith("#") and len(line) <= 200:
                out.append((title, line))
                break
    return out


def pages(roots):
    seen = {}
    for root in roots:
        for layout in LAYOUTS:
            for path in sorted(glob.glob(os.path.join(root, layout))):
                name = os.path.basename(path)[:-3]
                if not NAME.match(name) or name.lower() in seen:
                    continue
                text = open(path, encoding="utf-8", errors="ignore").read()
                synopsis = first_sentence(section(text, "SYNOPSIS"))
                if not synopsis or synopsis.lower().startswith(("this cmdlet is", "{{")):
                    continue
                module = os.path.basename(os.path.dirname(path))
                core = layout.startswith("reference/") or module in CLIENT_MODULES
                seen[name.lower()] = {"name": name, "module": module, "synopsis": synopsis, "examples": examples(text, name), "core": core}
    return seen


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("roots", nargs="+")
    ap.add_argument("--out", required=True)
    a = ap.parse_args()
    found = pages(a.roots)
    os.makedirs(a.out, exist_ok=True)
    train, test = [], []
    for key, p in found.items():
        if not p["core"]:
            continue
        rows = [{"tool": key, "page": key, "desc": p["synopsis"], "cmd": p["name"]}]
        rows += [{"tool": key, "page": key, "desc": t, "cmd": c} for t, c in p["examples"]]
        (test if held_out(key) else train).extend(rows)
    for name, part in (("ps_train", train), ("ps_test", test)):
        with open(os.path.join(a.out, f"{name}.jsonl"), "w", encoding="utf-8") as f:
            for r in part:
                f.write(json.dumps(r, ensure_ascii=False) + "\n")
    json.dump(found, open(os.path.join(a.out, "ps_pages.json"), "w", encoding="utf-8"), ensure_ascii=False)
    core = sum(p["core"] for p in found.values())
    print(f"{len(found)} cmdlets ({core} for training), {sum(len(p['examples']) for p in found.values())} examples; "
          f"training pairs: train {len(train)}, test {len(test)}")


if __name__ == "__main__":
    main()
