import sqlite3, os, collections

db = sqlite3.connect(os.path.expanduser("~/.reman/reman.db"))
cmds = [r[0] for r in db.execute("SELECT cmd_text FROM commands")]
total = len(cmds)

def toks(c):
    return c.strip().split()

progs = collections.Counter()
pairs = collections.Counter()   # (program, subcommand) -> how many unique commands
for c in cmds:
    t = toks(c)
    if not t:
        continue
    prog = t[0]
    progs[prog] += 1
    sub = ""
    if len(t) > 1 and not t[1].startswith("-") and "/" not in t[1] and "\\" not in t[1]:
        sub = t[1]
    pairs[(prog, sub)] += 1

print(f"unique commands:            {total}")
print(f"distinct programs:          {len(progs)}")
print(f"distinct (prog,sub) pairs:  {len(pairs)}   <- this is the number of --help fetches (cached)")
print()
print("Top 20 programs by # of unique commands (coverage concentration):")
covered = 0
for i, (p, n) in enumerate(progs.most_common(20), 1):
    covered += n
    print(f"  {i:>2}. {p:<28} {n:>4} cmds   (cumulative {100*covered/total:.0f}% of corpus)")
print()
# how many pairs to cover 80% / 90% of commands
cum = 0; n80 = n90 = None
for k, (pair, n) in enumerate(pairs.most_common(), 1):
    cum += n
    if n80 is None and cum >= 0.80*total: n80 = k
    if n90 is None and cum >= 0.90*total: n90 = k
print(f"(prog,sub) pairs needed to cover 80% of commands: {n80}")
print(f"(prog,sub) pairs needed to cover 90% of commands: {n90}")
