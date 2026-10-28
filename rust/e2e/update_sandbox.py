"""Updates, end to end in a sandbox: a fake ~/.reman (REMAN_HOME), a daemon on its own port, and a
release server on this machine standing in for GitHub, with its own signing key (REMAN_UPDATE_KEY,
made here with openssl). Your real install, profiles and history are never touched.

What may never be installed:
  * a version that isn't plain numbers (a path, text), the version you have, or an older one;
  * a release without a signature, signed by another key, or whose checksums changed after
    signing; an archive that doesn't match its signed checksum; an oversized download;
  * and the test server counts only on this machine (a URL that merely starts like one doesn't).
What happens:
  * a signed, newer release installs: `reman update` checks it, runs its setup, and the sandbox's
    daemon runs from its bin folder;
  * notify (`--auto off`; automatic is the default): the daemon tells about a new version once,
    suggesting `--auto on`;
  * an agent can't turn automatic updates on; `--auto off` is fine;
  * auto: the daemon installs a signed release on its own, once; a tampered one is refused and
    it says it couldn't update.

  python e2e/update_sandbox.py
"""
import hashlib, http.server, io, json, os, shutil, subprocess, sys, tarfile, tempfile, threading, time, zipfile

HERE = os.path.dirname(os.path.abspath(__file__))
WIN = os.name == "nt"
EXE_NAME = "reman.exe" if WIN else "reman"
BUILD = os.environ.get("REMAN_EXE_UNDER_TEST") or os.path.join(HERE, "..", "target", "release", EXE_NAME)
HOOK = os.path.join(os.path.dirname(BUILD), "reman-hook.exe" if WIN else "reman-hook")
PORT = 8797
if WIN:
    ARCHIVE = "reman-windows-x64.zip"
elif sys.platform == "darwin":
    ARCHIVE = "reman-macos-arm64.tar.gz"
else:
    ARCHIVE = "reman-linux-x64.tar.gz"
results = []


def check(name, ok, detail=""):
    ok = bool(ok)
    results.append(ok)
    print(f"  {'ok  ' if ok else 'FAIL'} {name}" + (f"  ({str(detail)[:500]})" if detail and not ok else ""))


def openssl(*args, data=None):
    return subprocess.run(["openssl", *args], input=data, capture_output=True, check=True).stdout


class Release:
    """What the fake GitHub serves: a tag and files by name."""

    def __init__(self):
        self.tag, self.files, self.asked = "v0.0.1", {}, 0


REL = Release()


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_GET(self):
        REL.asked += 1
        if self.path == "/repos/rdarshan10/reman/releases/latest":
            body = json.dumps({"tag_name": REL.tag, "html_url": "https://evil.example/x", "assets": [{"name": ARCHIVE, "browser_download_url": "https://evil.example/" + ARCHIVE}]}).encode()
        elif self.path.startswith(f"/download/{REL.tag}/") and self.path.rsplit("/", 1)[1] in REL.files:
            body = REL.files[self.path.rsplit("/", 1)[1]]
        else:
            self.send_response(404)
            self.end_headers()
            return
        self.send_response(200)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def archive_bytes():
    """The release archive, laid out like release.yml's: files at the root of the zip, in a
    folder named like the archive in a tarball. It holds this build."""
    buf = io.BytesIO()
    if WIN:
        with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as z:
            z.write(BUILD, "reman.exe")
            z.write(HOOK, "reman-hook.exe")
    else:
        stem = ARCHIVE[: -len(".tar.gz")]
        with tarfile.open(fileobj=buf, mode="w:gz") as t:
            t.add(BUILD, f"{stem}/reman")
            t.add(HOOK, f"{stem}/reman-hook")
    return buf.getvalue()


def sign(key, data, tmp):
    p = os.path.join(tmp, "msg")
    open(p, "wb").write(data)
    return openssl("pkeyutl", "-sign", "-rawin", "-inkey", key, "-in", p)


def publish(tag, key, tmp, archive, sums=None, sig_key=None, sig=True, tamper_sums=False):
    sums = sums or f"{hashlib.sha256(archive).hexdigest()}  {ARCHIVE}\n".encode()
    REL.tag, REL.files = tag, {ARCHIVE: archive, "SHA256SUMS.txt": sums}
    if sig:
        REL.files["SHA256SUMS.txt.sig"] = sign(sig_key or key, sums, tmp)
    if tamper_sums:
        REL.files["SHA256SUMS.txt"] = sums.replace(sums[:4], b"0000")


def main():
    tmp = tempfile.mkdtemp(prefix="reman-update-")
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    home = os.path.join(tmp, "home")
    try:
        run(tmp, home, f"http://127.0.0.1:{server.server_address[1]}")
    finally:
        env = sandbox_env(tmp, home, "")
        subprocess.run([BUILD, "stop"], env=env, capture_output=True)
        time.sleep(1)
        models = os.path.join(home, "models")
        if os.path.lexists(models):
            # the link only, never the real models behind it
            os.unlink(models) if os.path.islink(models) else os.rmdir(models)
        shutil.rmtree(tmp, ignore_errors=True)
        server.shutdown()
    passed = sum(results)
    print(f"\nUPDATE RESULT: {'PASS' if passed == len(results) else 'FAIL'} ({passed}/{len(results)})")
    sys.exit(0 if passed == len(results) else 1)


def sandbox_env(tmp, home, api, **extra):
    env = {k: v for k, v in os.environ.items() if k not in ("CLAUDECODE", "GEMINI_CLI") and not k.startswith("CODEX_")}
    env.update(REMAN_HOME=home, REMAN_DB=os.path.join(home, "reman.db"), REMAN_PORT=str(PORT), REMAN_NOTIFY_LOG=os.path.join(tmp, "notify.log"),
               REMAN_SPOOL=os.path.join(home, "spool.jsonl"), REMAN_UPDATE_API=api, REMAN_NO_CONNECT="1")
    env.pop("REMAN_CONFIG", None)
    env.update(extra)
    return {k: v for k, v in env.items() if v is not None}


HELD = []


def reman(env, *args, exe=None):
    """Run reman, and note it when its output stays held after it exits (a process it started
    inherited the pipe: whatever ran it, an agent's tool say, would wait for that process)."""
    p = subprocess.Popen([exe or BUILD, *args], env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    out = []
    reader = threading.Thread(target=lambda: out.append(p.stdout.read()), daemon=True)
    reader.start()
    try:
        code = p.wait(timeout=600)
    except subprocess.TimeoutExpired:
        p.kill()
        code = -1
    reader.join(15)
    if reader.is_alive():
        HELD.append(" ".join(args))
        return code, "(output still held by another process)"
    return code, out[0].decode("utf-8", "replace") if out else ""


def notes(tmp):
    p = os.path.join(tmp, "notify.log")
    return open(p, encoding="utf-8").read().splitlines() if os.path.exists(p) else []


def wait_for(cond, secs):
    end = time.time() + secs
    while time.time() < end:
        if cond():
            return True
        time.sleep(0.5)
    return cond()


def run(tmp, home, api):
    os.makedirs(home)
    # the search model, shared from your own install instead of downloaded again
    real_models = os.path.join(os.path.expanduser("~"), ".reman", "models")
    if os.path.isdir(real_models):
        if WIN:
            import _winapi
            _winapi.CreateJunction(real_models, os.path.join(home, "models"))
        else:
            os.symlink(real_models, os.path.join(home, "models"))
    key, other = os.path.join(tmp, "key.pem"), os.path.join(tmp, "other.pem")
    openssl("genpkey", "-algorithm", "ed25519", "-out", key)
    openssl("genpkey", "-algorithm", "ed25519", "-out", other)
    pub = openssl("pkey", "-in", key, "-pubout", "-outform", "DER")[-32:].hex()
    env = sandbox_env(tmp, home, api, REMAN_UPDATE_KEY=pub)
    bin_exe = os.path.join(home, "bin", EXE_NAME)
    version = reman(env, "--version")[1].split()[-1]
    good = archive_bytes()

    print("== never installed")
    cases = [
        ("a version that is a path", dict(tag="v../../evil"), "isn't one reman publishes"),
        ("the version you have", dict(tag="v" + version), "is the latest version"),
        ("an older version", dict(tag="v0.0.1"), "is the latest version"),
        ("no signature", dict(tag="v9.9.9", sig=False), "has no signature"),
        ("signed by another key", dict(tag="v9.9.9", sig_key=other), "aren't signed by reman's release key"),
        ("checksums changed after signing", dict(tag="v9.9.9", tamper_sums=True), "aren't signed by reman's release key"),
        ("an archive that isn't the signed one", dict(tag="v9.9.9", archive=good + b"x", sums=f"{hashlib.sha256(good).hexdigest()}  {ARCHIVE}\n".encode()), "checksum mismatch"),
        ("an oversized signature", dict(tag="v9.9.9"), "larger than any reman release"),
    ]
    for name, kw, want in cases:
        archive = kw.pop("archive", good)
        publish(key=key, tmp=tmp, archive=archive, **kw)
        if name == "an oversized signature":
            REL.files["SHA256SUMS.txt.sig"] = b"0" * (70 << 10)
        code, out = reman(env, "update")
        refused = want != "is the latest version"
        check(f"{name}: {'refused' if refused else 'nothing to do'}", (code != 0) == refused and want in out, out)
        check(f"{name}: nothing installed", not os.path.exists(bin_exe))

    publish("v9.9.9", key, tmp, good)
    # only this machine counts as the test server: these go to GitHub itself (and find no 9.9.9)
    port = api.rsplit(":", 1)[1]
    for fake in [f"http://localhost:@127.0.0.1:{port}", f"http://127.0.0.1.nip.io:{port}", f"https://127.0.0.1:{port}"]:
        before = REL.asked
        code, out = reman(sandbox_env(tmp, home, fake, REMAN_UPDATE_KEY=pub), "update", "--check")
        check(f"not the test server: {fake}", REL.asked == before and "9.9.9" not in out, out)

    print("== a signed, newer release")
    code, out = reman(env, "update")
    check("reman update installs it", code == 0 and "Signature and checksum OK" in out and "9.9.9 is installed" in out, out)
    check("into the sandbox's bin folder", os.path.exists(bin_exe))
    check("its address is GitHub's own, not the answer's", "github.com/rdarshan10/reman/releases/tag/v9.9.9" in out and "evil" not in out, out)
    code, out = reman(env, "ping")
    check("the sandbox's daemon is up", code == 0, out)
    check("reman update leaves no process holding its output", not HELD, HELD)

    print("== notify")
    reman(env, "stop")
    time.sleep(1)
    code, out = reman(env, "update", "--auto", "off")
    check("--auto off: told instead, written out (no setting means automatic)", code == 0 and '"notify"' in open(os.path.join(home, "config.json")).read(), out)
    open(os.path.join(home, "update.json"), "w").write("{}")
    if os.path.exists(os.path.join(tmp, "notify.log")):
        os.remove(os.path.join(tmp, "notify.log"))
    daemon = subprocess.Popen([bin_exe, "daemon", "--port", str(PORT)], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    told = lambda: [n for n in notes(tmp) if n.startswith("reman 9.9.9 is out")]
    check("the daemon tells about 9.9.9", wait_for(lambda: told(), 60), notes(tmp))
    check("and suggests automatic updates", any("reman update --auto on" in n for n in told()), told())
    time.sleep(6)
    check("once", len(told()) == 1, told())

    print("== who may turn automatic updates on")
    code, out = reman(sandbox_env(tmp, home, api, REMAN_UPDATE_KEY=pub, CLAUDECODE="1"), "update", "--auto", "on")
    cfg = os.path.join(home, "config.json")
    check("not an agent", code != 0 and "Claude Code" in out and '"auto"' not in (open(cfg).read() if os.path.exists(cfg) else ""), out)
    code, out = reman(sandbox_env(tmp, home, api, REMAN_UPDATE_KEY=pub, CLAUDECODE="1"), "update", "--auto", "off")
    check("an agent may turn them off", code == 0, out)

    print("== auto")
    publish("v9.9.10", key, tmp, good, tamper_sums=True)
    code, out = reman(env, "update", "--auto", "on")
    check("you may turn them on", code == 0 and '"auto"' in open(cfg).read(), out)
    failed = lambda: [n for n in notes(tmp) if n.startswith("reman couldn't update itself to 9.9.10")]
    check("a tampered release: it says it couldn't update", wait_for(lambda: failed(), 60), notes(tmp))
    check("and installs nothing", not os.path.exists(os.path.join(home, "update.log")))
    state = json.load(open(os.path.join(home, "update.json")))
    check("and tries it only once", state.get("tried") == "9.9.10", state)

    publish("v9.9.11", key, tmp, good)
    s = dict(state, checked=0)
    open(os.path.join(home, "update.json"), "w").write(json.dumps(s))
    log = os.path.join(home, "update.log")
    check("a signed release: it installs it on its own", wait_for(lambda: os.path.exists(log) and "binary" in open(log, encoding="utf-8", errors="replace").read(), 120),
          open(log, encoding="utf-8", errors="replace").read() if os.path.exists(log) else "no update.log")
    check("its daemon comes back", wait_for(lambda: reman(env, "ping")[0] == 0, 60))
    state = json.load(open(os.path.join(home, "update.json")))
    check("tried once, told once", state.get("tried") == "9.9.11" and state.get("told") == "9.9.11", state)
    check("no 'is out' note for a version it installs itself", not any(n.startswith("reman 9.9.11 is out") for n in notes(tmp)), notes(tmp))
    check("no reman command left its output held", not HELD, HELD)


if __name__ == "__main__":
    main()
