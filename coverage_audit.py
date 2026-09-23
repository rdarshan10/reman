import sqlite3, os, collections
from reman_enrich import parse_cmd

db = sqlite3.connect(os.path.expanduser("~/.reman/reman.db"))
described = db.execute("SELECT COUNT(*) FROM commands WHERE desc_source='tldr'").fetchone()[0]
raw_only  = db.execute("SELECT cmd_text FROM commands WHERE desc_source='none' OR desc_source IS NULL").fetchall()
total = described + len(raw_only)
print(f"described: {described}/{total} ({100*described/total:.0f}%)   raw-only: {len(raw_only)}\n")

# classify the raw-only long tail by parsed program
NON_RUNNABLE = {"#", "&", "cd", "if", "for", "while", "function", "async", "from", "import",
                "echo", "cls", "clear", "set", "return", "d:", "c:", "$", "{", "}", "(", ")", ""}
real_tool, junk = collections.Counter(), collections.Counter()
for (cmd,) in raw_only:
    prog, sub = parse_cmd(cmd)
    if cmd.strip().startswith("#") or prog in NON_RUNNABLE or prog is None \
       or prog.startswith((".", "/", "\\")) or ":" in (prog or "") or prog.isdigit():
        junk[prog or "(empty)"] += 1
    else:
        real_tool[prog] += 1

print(f"raw-only breakdown:  ~junk/non-tool = {sum(junk.values())}   looks-like-real-tool = {sum(real_tool.values())}\n")
print("Top 'real tool' programs with NO tldr description (the ones that matter for outer-world):")
for p, n in real_tool.most_common(20):
    print(f"  {n:>3}  {p}")
print("\nTop junk/non-runnable buckets (correctly left raw):")
for p, n in junk.most_common(8):
    print(f"  {n:>3}  {p}")
