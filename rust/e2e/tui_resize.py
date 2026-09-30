"""The finder at every window size, in a real terminal (ConPTY) read through a VT emulator, against
a sandbox daemon (own db, own port; your history is never touched):

  * launched at a size, the layout holds: hints on the first line, the query and tabs on the last,
    the card beside the list when wide and above it when narrow, a short note when too small;
  * resized while open (grow, shrink, wide <-> narrow, down to too small and back), the screen is
    exactly what a fresh launch at that size draws: no leftovers from the old size, nothing cut;
  * the help overlay and a long query survive resizing too, and the finder never crashes;
  * `reman settings` (sandboxed config) does the same, and a long folder path shows its end.

  python e2e/tui_resize.py
"""
import json, os, shutil, socket, subprocess, sys, tempfile, threading, time
import winpty, pyte

sys.stdout.reconfigure(encoding="utf-8")
HERE = os.path.dirname(os.path.abspath(__file__))
EXE = os.environ.get("REMAN_EXE_UNDER_TEST") or os.path.join(HERE, "..", "target", "release", "reman.exe")
PORT = 8797
CWD = r"C:\work\shop"
results = []

SIZES = [  # (rows, cols)
    (24, 80), (40, 120), (30, 200), (12, 118), (11, 117), (20, 60), (12, 40), (7, 24), (6, 30), (9, 23), (60, 140),
]


def check(name, ok, detail=""):
    results.append(bool(ok))
    print(f"  {'ok  ' if ok else 'FAIL'} {name}" + (f"\n{detail}" if detail and not ok else ""))


def call(obj, timeout=60):
    with socket.create_connection(("127.0.0.1", PORT), timeout=timeout) as s:
        s.sendall((json.dumps(obj) + "\n").encode())
        buf = b""
        while not buf.endswith(b"\n"):
            chunk = s.recv(1 << 20)
            if not chunk:
                break
            buf += chunk
    return json.loads(buf)


class Term:
    def __init__(self, rows, cols, *args, argv=None):
        self.rows, self.cols = rows, cols
        self.screen = pyte.Screen(cols, rows)
        self.stream = pyte.Stream(self.screen)
        self.lock = threading.Lock()
        self.p = winpty.PtyProcess.spawn(argv or [EXE, "find", "--cwd", CWD, *args], env=ENV, dimensions=(rows, cols))
        threading.Thread(target=self._pump, daemon=True).start()

    def _pump(self):
        while True:
            try:
                d = self.p.read(65536)
            except EOFError:
                return
            if "\x1b[c" in d:
                self.p.write("\x1b[?61;4;6;7;14;21;22;23;24;28;32;42c")
            with self.lock:
                self.stream.feed(d)

    def lines(self):
        with self.lock:
            return [l.rstrip() for l in self.screen.display]

    def cursor(self):
        with self.lock:
            return self.screen.cursor.y, self.screen.cursor.x

    def settle(self, quiet=0.8, limit=15):
        """Wait until the screen stops changing (results arrived, the frame is drawn)."""
        last, since, end = None, time.time(), time.time() + limit
        while time.time() < end:
            now = self.lines()
            if now != last:
                last, since = now, time.time()
            elif time.time() - since >= quiet and not any("searching" in l for l in now):
                break
            time.sleep(0.1)
        return self.lines()

    def resize(self, rows, cols):
        self.rows, self.cols = rows, cols
        with self.lock:
            self.screen.resize(rows, cols)
        self.p.setwinsize(rows, cols)

    def keys(self, s, wait=0.3):
        self.p.write(s)
        time.sleep(wait)

    def alive(self):
        return self.p.isalive()

    def close(self):
        try:
            self.p.write("\x1b")
            time.sleep(0.3)
        finally:
            if self.p.isalive():
                self.p.terminate(force=True)


def show(lines):
    return "\n".join(f"    |{l}" for l in lines)


def diff(a, b):
    out = []
    for i, (x, y) in enumerate(zip(a, b)):
        if x != y:
            out.append(f"    row {i}:\n      resized |{x}\n      fresh   |{y}")
    return "\n".join(out[:8])


def fresh(rows, cols, *args, keys=None, argv=None):
    t = Term(rows, cols, *args, argv=argv)
    t.settle()
    if keys:
        t.keys(keys)
    frame = t.settle(), t.cursor()
    t.close()
    return frame


def layout(lines, rows, cols, name):
    if cols < 24 or rows < 7:
        check(f"{name}: too small says so, whole", "terminal too small" in " ".join(l.strip() for l in lines), show(lines))
        return
    check(f"{name}: nothing drawn past the right edge", all(len(l) <= cols for l in lines), show(lines))
    check(f"{name}: hints on the first line", lines[0].strip() != "", show(lines))
    last = lines[rows - 1]
    check(f"{name}: query on the last line", last.startswith(" › "), show(lines))
    if cols >= 50:
        check(f"{name}: all three tabs show", all(t in last for t in ("Recall", "Fixes", "Flows")), show(lines))
    body = "\n".join(lines[1:rows - 2])
    check(f"{name}: results are listed", "docker" in body, show(lines))
    if cols >= 118 and rows >= 12:
        check(f"{name}: card beside the list", sum("│" in l for l in lines[1:rows - 2]) == rows - 3, show(lines))


def seed():
    base = int(time.time()) - 5 * 86400
    cmds = [
        "docker compose up -d", "docker compose down", "docker compose logs -f api", "docker ps -a",
        "docker build -t shop-api:latest .", "docker image prune -f", "git status", "git pull --rebase",
        "npm run dev", "npm test", "cargo build --release", "kubectl get pods -n shop",
        "docker run --rm -it -v ${PWD}:/app -w /app node:20-alpine sh -c \"npm ci && npm run build && npm run test -- --coverage\"",
        "echo 部署完成 && docker compose ps",
    ]
    for i, c in enumerate(cmds * 3):
        call({"op": "ingest", "command": c, "exit": 0 if i % 7 else 1, "cwd": CWD, "session": f"s{i // 10}", "actor": "human", "ts": base + i * 600, "duration_ms": 500})


ENV = {}


def main():
    tmp = tempfile.mkdtemp(prefix="reman-resize-")
    os.makedirs(os.path.join(tmp, "home", ".claude"))
    ENV.update(os.environ, REMAN_PORT=str(PORT), REMAN_CONNECT_HOME=os.path.join(tmp, "home"), REMAN_CONFIG=os.path.join(tmp, "config.json"))
    env = dict(ENV, REMAN_DB=os.path.join(tmp, "reman.db"), REMAN_SPOOL=os.path.join(tmp, "spool.jsonl"))
    proc = subprocess.Popen([EXE, "daemon", "--port", str(PORT)], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(240):
            try:
                call({"op": "ping"}, timeout=2)
                break
            except OSError:
                time.sleep(0.5)
        seed()
        run()
    finally:
        try:
            call({"op": "shutdown"}, timeout=5)
        except OSError:
            pass
        proc.kill()
        time.sleep(0.5)
        shutil.rmtree(tmp, ignore_errors=True)
    passed = sum(results)
    print(f"\nRESIZE RESULT: {'PASS' if passed == len(results) else 'FAIL'} ({passed}/{len(results)})")
    return 0 if passed == len(results) else 1


def run():
    print("launched at each size:")
    frames = {}
    for rows, cols in SIZES:
        frames[(rows, cols)] = fresh(rows, cols, "docker")
        layout(frames[(rows, cols)][0], rows, cols, f"{cols}x{rows}")

    print("\nresized while open, compared with a fresh launch at that size:")
    t = Term(*SIZES[0], "docker")
    t.settle()
    path = SIZES[1:] + [SIZES[0], (40, 120), (7, 24), (60, 140)]
    for rows, cols in path:
        t.resize(rows, cols)
        got, cur = t.settle()[:], t.cursor()
        want, want_cur = frames[(rows, cols)]
        check(f"-> {cols}x{rows}: same as a fresh launch", got == want, diff(got, want) or show(got))
        if cols >= 24 and rows >= 7:
            check(f"-> {cols}x{rows}: cursor in the query", cur == want_cur, f"    {cur} vs {want_cur}")
        check(f"-> {cols}x{rows}: still running", t.alive())
    t.close()

    print("\nthe help overlay, resized:")
    t = Term(40, 120, "docker")
    t.settle()
    t.keys("\x1bOP")  # F1
    for rows, cols in [(20, 60), (40, 120), (7, 24), (30, 90)]:
        t.resize(rows, cols)
        got = t.settle()
        want, _ = fresh(rows, cols, "docker", keys="\x1bOP")
        check(f"help at {cols}x{rows}: same as a fresh launch", got == want, diff(got, want) or show(got))
    check("help: still running", t.alive())
    t.close()

    print("\na long query, resized:")
    q = "docker run --rm -it node:20-alpine npm ci and then the build and the tests with coverage 部署"
    t = Term(24, 80, q)
    t.settle()
    for rows, cols in [(24, 40), (24, 120), (12, 30)]:
        t.resize(rows, cols)
        got, cur = t.settle(), t.cursor()
        want, want_cur = fresh(rows, cols, q)
        check(f"long query at {cols}x{rows}: same as a fresh launch", got == want, diff(got, want) or show(got))
        check(f"long query at {cols}x{rows}: cursor on the last line, inside the query", cur[0] == rows - 1 and cur[1] < cols - 20, f"    {cur}\n{show(got)}")
    t.close()

    print("\nreman settings, resized:")
    settings = [EXE, "settings"]
    t = Term(30, 100, argv=settings)
    t.settle()
    for rows, cols in [(40, 140), (12, 40), (11, 39), (8, 20), (20, 70), (30, 100)]:
        t.resize(rows, cols)
        got = t.settle()
        want, _ = fresh(rows, cols, argv=settings)
        check(f"settings at {cols}x{rows}: same as a fresh launch", got == want, diff(got, want) or show(got))
        if cols < 40 or rows < 12:
            check(f"settings at {cols}x{rows}: the whole note shows", "bigger" in " ".join(got), show(got))
        else:
            check(f"settings at {cols}x{rows}: every key fits on the last line", "esc close" in got[rows - 1], show(got))
            check(f"settings at {cols}x{rows}: names never run into their state", not any("CLInot" in l or "…not" in l for l in got), show(got))
    check("settings: still running", t.alive())
    t.close()

    print("\nsettings, a long folder path typed in a narrow window:")
    t = Term(20, 60, argv=settings)
    t.settle()
    t.keys("a")
    path = r"C:\some\very\long\folder\path\that\goes\past\the\edge\END"
    t.keys(path, wait=0.8)
    got, cur = t.settle(), t.cursor()
    row = next((l for l in got if "folder to share" in l), "")
    check("the end of the path shows", row.endswith("edge\\END"), show(got))
    check("the cursor sits right after it", cur == (got.index(row), len(row)) if row else False, f"    {cur}\n{show(got)}")
    t.keys("\x1b")
    t.close()


if __name__ == "__main__":
    sys.exit(main())
