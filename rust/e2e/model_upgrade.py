"""Getting reman's own embedding model and moving a history to it, in a sandbox (own home, own
port, a model server on this machine; your history and models are never touched):

  * the daemon downloads the model, checks it against the checksum it was given, unpacks it and
    re-embeds the history with it: the database records the model, search still finds things;
  * a download whose checksum is wrong is never used: the history stays on the base model;
  * with updates off nothing is downloaded at all (the server sees no request);
  * `"embed_model": "base"` downloads nothing, and moves a history that was on reman's model
    back to the base model.

The "model" served is the base model's own files repackaged (the real one is only a different
model.onnx), so this needs no download: only ~/.reman/models with bge-small-en-v1.5 in it. With
--zip, the real release zip instead (before uploading it).

  python e2e/model_upgrade.py [--zip reman-commands-1.zip]
"""
import glob, hashlib, http.server, json, os, shutil, socket, subprocess, sys, tempfile, threading, time, zipfile

HERE = os.path.dirname(os.path.abspath(__file__))
EXE = os.environ.get("REMAN_EXE_UNDER_TEST") or os.path.join(HERE, "..", "target", "release", "reman.exe" if os.name == "nt" else "reman")
PORT = 8786
REAL_MODELS = os.path.join(os.path.expanduser("~"), ".reman", "models")
BASE_CACHE = "models--Xenova--bge-small-en-v1.5"
MODEL_ID = "reman-commands-1"
COMMANDS = [("docker compose up -d", r"C:\work\api"), ("git branch -D old-feature", r"C:\work\api"), ("npm install", r"C:\work\web"),
            ("pytest -x -q", r"C:\work\api"), ("mkdir reports", r"C:\work"), ("ssh deploy@staging.internal", r"C:\work")]

results = []
requests = []


def check(name, ok, detail=""):
    results.append(ok)
    print(f"  {'ok  ' if ok else 'FAIL'} {name}" + (f"  ({detail})" if detail and not ok else ""))


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


def model_zip(path):
    """The base model's files as reman's model zip (model.onnx, tokenizer files, reman.json)."""
    snap = glob.glob(os.path.join(REAL_MODELS, BASE_CACHE, "snapshots", "*"))[0]
    with zipfile.ZipFile(path, "w", zipfile.ZIP_STORED) as z:
        z.write(os.path.join(snap, "onnx", "model.onnx"), "model.onnx")
        for f in ("tokenizer.json", "tokenizer_config.json", "special_tokens_map.json", "config.json"):
            z.write(os.path.join(snap, f), f)
        z.writestr("reman.json", json.dumps({"close_shift": 0.0}))
    return hashlib.sha256(open(path, "rb").read()).hexdigest()


def serve(root):
    class Handler(http.server.SimpleHTTPRequestHandler):
        def __init__(self, *a, **k):
            super().__init__(*a, directory=root, **k)

        def log_message(self, *a):
            requests.append(self.path)

    srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv


class Sandbox:
    """A reman home of its own: models/ holds a link to the base model's cache only, so what the
    daemon downloads lands here, never in the real models folder."""

    def __init__(self, settings):
        self.home = tempfile.mkdtemp(prefix="reman-model-")
        os.makedirs(os.path.join(self.home, "models"))
        self.link = os.path.join(self.home, "models", BASE_CACHE)
        if os.name == "nt":
            import _winapi
            _winapi.CreateJunction(os.path.join(REAL_MODELS, BASE_CACHE), self.link)
        else:
            os.symlink(os.path.join(REAL_MODELS, BASE_CACHE), self.link)
        self.write_settings(settings)
        self.proc = None

    def write_settings(self, settings):
        with open(os.path.join(self.home, "config.json"), "w") as f:
            json.dump(settings, f)

    def start(self, extra_env):
        env = dict(os.environ, REMAN_HOME=self.home, REMAN_PORT=str(PORT), **extra_env)
        env.pop("REMAN_MODEL_DIR", None)
        self.proc = subprocess.Popen([EXE, "daemon", "--port", str(PORT)], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(240):
            try:
                return call({"op": "ping"}, timeout=2)
            except OSError:
                time.sleep(0.5)

    def stop(self):
        try:
            call({"op": "shutdown"}, timeout=5)
        except OSError:
            pass
        if self.proc:
            self.proc.wait(30)
        self.proc = None

    def log(self):
        try:
            return open(os.path.join(self.home, "daemon.log"), encoding="utf-8", errors="replace").read()
        except OSError:
            return ""

    def close(self):
        self.stop()
        # the link first: a tree removal must never reach the real model files behind it
        os.unlink(self.link) if os.path.islink(self.link) else os.rmdir(self.link)
        shutil.rmtree(self.home, ignore_errors=True)


def wait_for(pred, secs=300):
    end = time.time() + secs
    while time.time() < end:
        r = pred()
        if r:
            return r
        time.sleep(1)
    return None


def main():
    if not glob.glob(os.path.join(REAL_MODELS, BASE_CACHE, "snapshots", "*", "onnx", "model.onnx")):
        print("needs bge-small-en-v1.5 in ~/.reman/models (run reman once)")
        return 1
    files = tempfile.mkdtemp(prefix="reman-model-srv-")
    if "--zip" in sys.argv:
        shutil.copy(sys.argv[sys.argv.index("--zip") + 1], os.path.join(files, "model.zip"))
        good = hashlib.sha256(open(os.path.join(files, "model.zip"), "rb").read()).hexdigest()
        print(f"serving {sys.argv[sys.argv.index('--zip') + 1]} (sha256 {good[:12]}...)")
    else:
        good = model_zip(os.path.join(files, "model.zip"))
    srv = serve(files)
    url = f"http://127.0.0.1:{srv.server_address[1]}/model.zip"
    ok_env = {"REMAN_MODEL_URL": url, "REMAN_MODEL_SHA256": good}
    try:
        print("downloading and moving a history to reman's model")
        # a history recorded on the base model first (reman's model turned off), then turned on
        sb = Sandbox({"embed_model": "base"})
        try:
            p = sb.start(ok_env)
            for cmd, cwd in COMMANDS:
                call({"op": "ingest", "command": cmd, "exit": 0, "cwd": cwd, "session": "s", "actor": "human"})
            check("a fresh history starts on the base model", p.get("model") == "bge-small-en-v1.5", p)
            sb.stop()
            sb.write_settings({})
            p = sb.start(ok_env)  # upgrade_model runs at start
            moved = wait_for(lambda: call({"op": "ping"}).get("model") == MODEL_ID)
            check("the daemon moves the history to reman's model", bool(moved), sb.log()[-600:])
            check("the model is unpacked and marked checked", os.path.exists(os.path.join(sb.home, "models", MODEL_ID, ".verified")))
            check("the real models folder is untouched", not os.path.exists(os.path.join(REAL_MODELS, MODEL_ID)))
            r = call({"op": "search", "query": "start the containers", "k": 3})
            check("search finds things with it", r["results"] and r["results"][0]["command"] == "docker compose up -d", r["results"][:1])
            check("the log says what it did", "re-embedded" in sb.log() and MODEL_ID in sb.log())
            sb.stop()
            n = len(requests)
            p = sb.start(ok_env)
            time.sleep(3)
            check("a restart downloads nothing again", len(requests) == n and p.get("model") == MODEL_ID, requests[n:])

            print("\nbacking out: \"embed_model\": \"base\"")
            sb.stop()
            sb.write_settings({"embed_model": "base"})
            sb.start(ok_env)
            back = wait_for(lambda: call({"op": "ping"}).get("model") == "bge-small-en-v1.5")
            check("the history moves back to the base model", bool(back), sb.log()[-400:])
            r = call({"op": "search", "query": "delete a local branch", "k": 3})
            check("search still finds things", r["results"] and r["results"][0]["command"] == "git branch -D old-feature", r["results"][:1])
        finally:
            sb.close()

        print("\na download that isn't the model reman knows")
        sb = Sandbox({})
        try:
            sb.start({"REMAN_MODEL_URL": url, "REMAN_MODEL_SHA256": "0" * 63 + "1"})
            refused = wait_for(lambda: "not the one reman knows" in sb.log(), 120)
            check("is refused", bool(refused), sb.log()[-400:])
            check("and never unpacked", not os.path.exists(os.path.join(sb.home, "models", MODEL_ID)))
            check("the history stays on the base model", call({"op": "ping"}).get("model") == "bge-small-en-v1.5")
        finally:
            sb.close()

        print("\nupdates off: reman stays off the network")
        sb = Sandbox({"updates": "off"})
        try:
            n = len(requests)
            sb.start(ok_env)
            time.sleep(8)
            check("nothing is downloaded", len(requests) == n, requests[n:])
            check("the history stays on the base model", call({"op": "ping"}).get("model") == "bge-small-en-v1.5")
        finally:
            sb.close()
    finally:
        srv.shutdown()
        shutil.rmtree(files, ignore_errors=True)
    passed = sum(results)
    print(f"\nMODEL RESULT: {'PASS' if passed == len(results) else 'FAIL'} ({passed}/{len(results)})")
    return 0 if passed == len(results) else 1


if __name__ == "__main__":
    sys.exit(main())
