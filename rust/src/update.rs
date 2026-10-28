//! Updates. Once a day the daemon asks GitHub for the latest release (nothing is sent but the
//! request itself). What it does then is the `updates` setting:
//!   * absent or "auto" (the default): downloads it and installs it with the new binary's own
//!     `reman setup --no-connect`; the new daemon then says so;
//!   * "notify": one desktop notification per new version, suggesting automatic updates;
//!   * "off": no check at all.
//! `reman update` installs the latest release now, whatever the setting.
//!
//! Nothing runs unless it is a release signed with reman's release key:
//!   * from GitHub's answer only the version is used, and only if it is plain numbers; every URL is
//!     built here, on github.com/rdarshan10/reman, over https only (redirects too);
//!   * SHA256SUMS.txt must carry a valid Ed25519 signature (SHA256SUMS.txt.sig) by the key below,
//!     whose private half signs releases in CI; then the archive must match its SHA-256;
//!   * only then is it unpacked, into ~/.reman/update (the user's own folder), and run;
//!   * only a newer version, never a downgrade; downloads have size limits;
//!   * an agent can't turn automatic updates on.
//!
//! State lives in ~/.reman/update.json. The test suites point the check at their own server with
//! REMAN_UPDATE_API and their own key with REMAN_UPDATE_KEY; both count only on a loopback address
//! with REMAN_HOME set, so they can never replace the real install.

use crate::{config, notify, settings};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;

const REPO: &str = "rdarshan10/reman";
const DAY: i64 = 86_400;
/// reman's release key (Ed25519, public half). The private half is the REMAN_SIGNING_KEY secret
/// that release.yml signs SHA256SUMS.txt with.
const RELEASE_KEY: [u8; 32] = hex32("e1671e8ca4d5ee292b496af5035783f1ecf7b0984bcbdeaad70fb78304409ba8");
const MAX_ARCHIVE: u64 = 200 << 20;
const MAX_SMALL: u64 = 64 << 10;

const fn hex32(s: &str) -> [u8; 32] {
    let b = s.as_bytes();
    let mut out = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        out[i] = (nib(b[2 * i]) << 4) | nib(b[2 * i + 1]);
        i += 1;
    }
    out
}

const fn nib(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        _ => panic!("not lowercase hex"),
    }
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    /// when the last check ran
    #[serde(default)]
    checked: i64,
    /// the newest version that check found
    #[serde(default, skip_serializing_if = "Option::is_none")]
    latest: Option<String>,
    /// the newest version a notification already told about
    #[serde(default, skip_serializing_if = "Option::is_none")]
    told: Option<String>,
    /// the newest version it tried to install on its own (once each)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tried: Option<String>,
    /// set just before installing on its own: the version it updated from
    #[serde(default, skip_serializing_if = "Option::is_none")]
    from: Option<String>,
}

fn state_path() -> PathBuf {
    config::home().join("update.json")
}

impl State {
    fn load() -> State {
        std::fs::read(state_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }
    fn save(&self) {
        if let Ok(b) = serde_json::to_vec_pretty(self) {
            let _ = std::fs::write(state_path(), b);
        }
    }
}

pub struct Release {
    pub version: String,
}

impl Release {
    /// One of this release's files, at an address built here, never taken from the answer.
    fn file(&self, name: &str) -> String {
        match test_server() {
            Some(base) => format!("{base}/download/v{}/{name}", self.version),
            None => format!("https://github.com/{REPO}/releases/download/v{}/{name}", self.version),
        }
    }
    pub fn page(&self) -> String {
        format!("https://github.com/{REPO}/releases/tag/v{}", self.version)
    }
}

/// A release version as reman publishes them: 2 to 4 parts, numbers only. Anything else in the
/// answer (a path, a URL, text) is refused rather than used.
fn valid_version(v: &str) -> bool {
    let parts: Vec<&str> = v.split('.').collect();
    (2..=4).contains(&parts.len()) && parts.iter().all(|p| !p.is_empty() && p.len() <= 6 && p.bytes().all(|b| b.is_ascii_digit()))
}

/// The test suites' server: honoured only for a sandbox (REMAN_HOME set) on this machine.
fn test_server() -> Option<String> {
    let url = std::env::var("REMAN_UPDATE_API").ok()?;
    (on_this_machine(&url) && std::env::var_os("REMAN_HOME").is_some_and(|h| !h.is_empty())).then(|| url.trim_end_matches('/').to_string())
}

/// http://127.0.0.1:<port> or http://localhost:<port>, nothing else (`http://localhost:@elsewhere/`
/// is elsewhere).
pub(crate) fn on_this_machine(url: &str) -> bool {
    ["http://127.0.0.1:", "http://localhost:"].iter().any(|p| {
        url.strip_prefix(p).is_some_and(|rest| {
            let port = rest.split('/').next().unwrap_or("");
            !port.is_empty() && port.len() <= 5 && port.bytes().all(|b| b.is_ascii_digit())
        })
    })
}

fn release_key() -> [u8; 32] {
    if test_server().is_some() {
        if let Some(k) = std::env::var("REMAN_UPDATE_KEY").ok().filter(|k| k.len() == 64 && k.bytes().all(|b| b.is_ascii_hexdigit())) {
            let mut out = [0u8; 32];
            for (i, o) in out.iter_mut().enumerate() {
                *o = u8::from_str_radix(&k[2 * i..2 * i + 2], 16).unwrap_or(0);
            }
            return out;
        }
    }
    RELEASE_KEY
}

/// Whether `sig` is the key's signature of `msg`.
fn signed(msg: &[u8], sig: &[u8], key: &[u8; 32]) -> bool {
    let (Ok(key), Ok(sig)) = (ed25519_dalek::VerifyingKey::from_bytes(key), <[u8; 64]>::try_from(sig)) else { return false };
    key.verify_strict(msg, &ed25519_dalek::Signature::from_bytes(&sig)).is_ok()
}

/// This system's archive among a release's files; None where no build is published.
fn archive_name() -> Option<&'static str> {
    if cfg!(all(windows, target_arch = "x86_64")) {
        Some("reman-windows-x64.zip")
    } else if cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        Some("reman-macos-arm64.tar.gz")
    } else if cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        Some("reman-linux-x64.tar.gz")
    } else {
        None
    }
}

/// "0.3.10" is newer than "0.3.9"; a leading `v` and anything after a `-` are ignored.
pub fn newer(candidate: &str, than: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> { v.trim().trim_start_matches('v').split('-').next().unwrap_or("").split('.').map(|p| p.parse().unwrap_or(0)).collect() };
    let (a, b) = (parts(candidate), parts(than));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

fn agent(timeout: Duration) -> ureq::Agent {
    // https only, redirects included (GitHub sends downloads on to its file servers)
    ureq::Agent::config_builder().timeout_global(Some(timeout)).https_only(test_server().is_none()).build().into()
}

fn api() -> String {
    test_server().unwrap_or_else(|| "https://api.github.com".into())
}

fn user_agent() -> String {
    format!("reman/{}", config::VERSION)
}

/// The latest published release (GitHub's `latest` is never a draft or a pre-release).
pub fn latest() -> Result<Release> {
    let url = format!("{}/repos/{REPO}/releases/latest", api());
    let v: serde_json::Value = agent(Duration::from_secs(20))
        .get(&url)
        .header("User-Agent", user_agent())
        .header("Accept", "application/vnd.github+json")
        .call()
        .context("asking GitHub for the latest release")?
        .body_mut()
        .with_config()
        .limit(MAX_SMALL * 16)
        .read_json()?;
    let tag = v["tag_name"].as_str().context("the release has no version")?;
    let version = tag.strip_prefix('v').unwrap_or(tag);
    if !valid_version(version) {
        bail!("the latest release's version isn't one reman publishes ({tag:?}); not using it");
    }
    Ok(Release { version: version.to_string() })
}

/// A release file that isn't there.
#[derive(Debug)]
struct Missing;

impl std::fmt::Display for Missing {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("not in the release")
    }
}

impl std::error::Error for Missing {}

/// At most `limit` bytes of `url`, into `to`. https only, except a test suite's server on this
/// machine for a sandbox (REMAN_HOME set).
pub(crate) fn download(url: &str, to: &Path, limit: u64) -> Result<()> {
    use std::io::Read;
    let sandbox = std::env::var_os("REMAN_HOME").is_some_and(|h| !h.is_empty());
    let local = sandbox && on_this_machine(url);
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(600)))
        .https_only(test_server().is_none() && !local)
        .build()
        .into();
    let resp = match agent.get(url).header("User-Agent", user_agent()).call() {
        Ok(r) => r,
        Err(ureq::Error::StatusCode(404)) => return Err(Missing.into()),
        Err(e) => return Err(e).with_context(|| format!("downloading {url}")),
    };
    let mut f = std::fs::File::create(to)?;
    let n = std::io::copy(&mut resp.into_body().into_reader().take(limit + 1), &mut f).with_context(|| format!("downloading {url}"))?;
    if n > limit {
        drop(f);
        let _ = std::fs::remove_file(to);
        bail!("{url} is larger than any reman release; not using it");
    }
    Ok(())
}

/// The expected SHA-256 of `name` in a SHA256SUMS.txt (`<hex>  name` or `<hex> *name`).
fn expected_sum(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        let (sum, file) = (it.next()?, it.next()?);
        (file.trim_start_matches('*') == name).then(|| sum.to_ascii_lowercase())
    })
}

pub(crate) fn sha256_of(p: &Path) -> Result<String> {
    use std::io::Read;
    let (mut h, mut f, mut buf) = (Sha256::new(), std::fs::File::open(p)?, vec![0u8; 1 << 16]);
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

pub(crate) fn unpack(archive: &Path, into: &Path) -> Result<()> {
    // the system's own tar, not whatever a folder on PATH calls tar (Windows 10 and 11 ship
    // bsdtar, which reads zip too); both refuse entries that climb out with `..`
    let tar = if cfg!(windows) {
        std::env::var_os("SystemRoot").map(|r| PathBuf::from(r).join("System32").join("tar.exe")).filter(|p| p.exists()).unwrap_or_else(|| "tar".into())
    } else {
        ["/usr/bin/tar", "/bin/tar"].iter().map(PathBuf::from).find(|p| p.exists()).unwrap_or_else(|| "tar".into())
    };
    let ok = std::process::Command::new(&tar).arg("-xf").arg(archive).arg("-C").arg(into).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().is_ok_and(|s| s.success());
    if !ok && cfg!(windows) {
        let ps = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", "Expand-Archive -LiteralPath $env:REMAN_ARCHIVE -DestinationPath $env:REMAN_INTO -Force"])
            .env("REMAN_ARCHIVE", archive)
            .env("REMAN_INTO", into)
            .status()?;
        if ps.success() {
            return Ok(());
        }
    }
    if !ok {
        bail!("could not unpack {}", archive.display());
    }
    Ok(())
}

/// Download, verify and unpack a release; the path of its `reman` binary, ready to run `setup`.
fn fetch(r: &Release) -> Result<PathBuf> {
    let Some(name) = archive_name() else { bail!("reman publishes no build for this system; build {} from source", r.version) };
    let root = config::home().join("update");
    // earlier updates' files (one still running stays, harmlessly)
    if let Ok(rd) = std::fs::read_dir(&root) {
        for e in rd.flatten() {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
    let dir = root.join(&r.version);
    std::fs::create_dir_all(&dir)?;
    // 1. the checksums, signed by reman's release key
    let (sums, sig) = (dir.join("SHA256SUMS.txt"), dir.join("SHA256SUMS.txt.sig"));
    download(&r.file("SHA256SUMS.txt"), &sums, MAX_SMALL)?;
    if let Err(e) = download(&r.file("SHA256SUMS.txt.sig"), &sig, MAX_SMALL) {
        if e.is::<Missing>() {
            bail!("reman {} has no signature; not installing it", r.version);
        }
        return Err(e);
    }
    let sums_bytes = std::fs::read(&sums)?;
    if !signed(&sums_bytes, &std::fs::read(&sig)?, &release_key()) {
        bail!("reman {}'s checksums aren't signed by reman's release key; not installing it", r.version);
    }
    // 2. the archive, matching its signed checksum
    let want = expected_sum(&String::from_utf8_lossy(&sums_bytes), name).with_context(|| format!("the signed checksums don't list {name}"))?;
    let archive = dir.join(name);
    download(&r.file(name), &archive, MAX_ARCHIVE)?;
    let got = sha256_of(&archive)?;
    if got != want {
        bail!("checksum mismatch for {name} (expected {want}, got {got}); not installing it");
    }
    // 3. only now unpacked
    unpack(&archive, &dir)?;
    let exe = if cfg!(windows) { "reman.exe" } else { "reman" };
    // the Windows zip holds the files themselves, the tarballs a folder named like the archive
    let stem = name.trim_end_matches(".zip").trim_end_matches(".tar.gz");
    [dir.join(exe), dir.join(stem).join(exe)].into_iter().find(|p| p.exists()).context("the download has no reman binary")
}

/// The new version's setup: its binary and daemon, the coding tools left as they are. A test's
/// sandbox leaves the real shell profiles alone too.
fn setup_args() -> Vec<&'static str> {
    let mut a = vec!["setup", "--no-connect"];
    if test_server().is_some() {
        a.push("--no-profile");
    }
    a
}

/// `reman update`: install the latest release now.
pub fn install_now() -> Result<()> {
    let r = latest()?;
    if !newer(&r.version, config::VERSION) {
        println!("reman {} is the latest version.", config::VERSION);
        return Ok(());
    }
    println!("Updating reman {} -> {}...", config::VERSION, r.version);
    let exe = fetch(&r)?;
    println!("Signature and checksum OK.");
    // setup gets this process's output through its own copies of it; the originals mustn't reach
    // the daemon setup starts, or whatever runs `reman update` (an agent, a script) waits on them
    // until that daemon exits
    #[cfg(windows)]
    crate::client::stop_std_handle_inheritance();
    let st = std::process::Command::new(&exe).args(setup_args()).status().context("running the new version's setup")?;
    if !st.success() {
        bail!("the new version's setup failed ({st})");
    }
    let mut s = State::load();
    s.told = Some(r.version.clone());
    s.latest = Some(r.version.clone());
    s.save();
    println!("\nreman {} is installed. What's new: {}", r.version, r.page());
    if settings::load().updates_mode() == "notify" {
        println!("To keep reman up to date on its own: reman update --auto on");
    }
    Ok(())
}

/// `reman update --check`
pub fn check_now() -> Result<()> {
    let r = latest()?;
    let mut s = State::load();
    (s.checked, s.latest) = (config::now(), Some(r.version.clone()));
    s.save();
    if newer(&r.version, config::VERSION) {
        println!("reman {} is out (you have {}): {}", r.version, config::VERSION, r.page());
        println!("Install it: reman update");
        if settings::load().updates_mode() != "auto" {
            println!("Or have reman update itself from now on: reman update --auto on");
        }
    } else {
        println!("reman {} is the latest version.", config::VERSION);
    }
    Ok(())
}

/// The newer version a past check found, if one is waiting (for `reman settings`).
pub fn waiting() -> Option<String> {
    State::load().latest.filter(|v| valid_version(v) && newer(v, config::VERSION))
}

/// Whether this daemon checks for updates at all: never on a sandbox database, unless a test
/// points it at its own server.
pub fn daemon_checks() -> bool {
    test_server().is_some() || std::env::var_os("REMAN_DB").is_none()
}

/// The daemon's update thread: says so after an update it installed itself, then checks daily.
pub fn watch() {
    let mut s = State::load();
    if let Some(from) = s.from.take() {
        if from != config::VERSION && valid_version(&from) {
            notify::send(&format!("reman updated to {}", config::VERSION), &format!("from {from}. Change how reman updates in reman settings."));
            crate::daemon::log(&format!("updated itself from {from} to {}", config::VERSION));
        }
        s.save();
    }
    let test = test_server().is_some();
    // not while the daemon is still warming up, and not for a daemon that only lives a moment
    std::thread::sleep(Duration::from_secs(if test { 1 } else { 300 }));
    loop {
        let mode = settings::load().updates_mode();
        if mode != "off" && config::now() - State::load().checked >= DAY {
            if let Err(e) = check_and_act(mode) {
                crate::daemon::log(&format!("update check: {e:#}"));
            }
        }
        std::thread::sleep(Duration::from_secs(if test { 2 } else { 3600 }));
    }
}

fn check_and_act(mode: &str) -> Result<()> {
    let mut s = State::load();
    s.checked = config::now();
    s.save();
    let r = latest()?;
    s.latest = Some(r.version.clone());
    s.save();
    if !newer(&r.version, config::VERSION) {
        return Ok(());
    }
    // only the installed copy updates itself: a daemon run from a build folder is someone working on reman
    let installed = config::bin_dir().join(if cfg!(windows) { "reman.exe" } else { "reman" });
    let me_installed = std::env::current_exe().ok().and_then(|p| p.canonicalize().ok()) == installed.canonicalize().ok();
    if mode == "auto" && me_installed && s.tried.as_deref() != Some(r.version.as_str()) {
        (s.tried, s.told) = (Some(r.version.clone()), Some(r.version.clone()));
        s.save();
        match fetch(&r).and_then(|exe| start_setup(&exe, &mut s)) {
            Ok(()) => return Ok(()),
            Err(e) => {
                notify::send(&format!("reman couldn't update itself to {}", r.version), "Run reman update in a terminal to see why.");
                return Err(e);
            }
        }
    }
    if s.told.as_deref() == Some(r.version.as_str()) {
        return Ok(());
    }
    s.told = Some(r.version.clone());
    s.save();
    notify::send(&format!("reman {} is out", r.version), "Run reman update to install it. To keep reman up to date on its own: reman update --auto on");
    Ok(())
}

/// The new binary's setup replaces this daemon (it stops it first), so it runs on its own.
fn start_setup(exe: &Path, s: &mut State) -> Result<()> {
    s.from = Some(config::VERSION.to_string());
    s.save();
    let log = std::fs::File::create(config::home().join("update.log"))?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(setup_args()).stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log);
    crate::daemon::log(&format!("updating itself with {}", exe.display()));
    crate::client::spawn_detached(cmd)
}

/// `reman update --auto on|off`
pub fn set_auto(on: bool) -> Result<()> {
    if on {
        crate::connect::refuse_if_agent("Turning on automatic updates")?;
    }
    let mut st = settings::load();
    // "notify" is written out: absent means automatic
    st.updates = Some(if on { "auto" } else { "notify" }.to_string());
    settings::save(&st)?;
    // the daemon looks again within the hour, not tomorrow
    let mut s = State::load();
    s.checked = 0;
    s.save();
    println!(
        "{}",
        if on {
            "reman now installs new versions on its own, once their signature and checksum check out, and tells you when it did."
        } else {
            "reman tells you when a new version is out; it installs nothing on its own."
        }
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_number() {
        assert!(newer("0.3.10", "0.3.9"));
        assert!(newer("v0.4.0", "0.3.3"));
        assert!(newer("1.0", "0.9.9"));
        assert!(!newer("0.3.3", "0.3.3"));
        assert!(!newer("0.3.2", "0.3.3"));
        assert!(!newer("0.3.3-rc1", "0.3.3"));
    }

    #[test]
    fn only_plain_versions() {
        assert!(valid_version("0.3.4") && valid_version("1.0") && valid_version("10.20.30.40"));
        for bad in ["", "0", "../../bin/x", "0.3.4/../x", "0.3.4-rc1", "0.3.x", "1.2.3.4.5", "0..3", "1234567.0", "0.3.4 ", "https://evil/0.4"] {
            assert!(!valid_version(bad), "{bad:?}");
        }
    }

    #[test]
    fn only_the_keys_signature_counts() {
        use ed25519_dalek::Signer;
        let sk = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let pk = sk.verifying_key().to_bytes();
        let msg = b"abc  reman-windows-x64.zip\n";
        let sig = sk.sign(msg).to_bytes();
        assert!(signed(msg, &sig, &pk));
        // a changed byte, another key, a short or empty signature: all refused
        assert!(!signed(b"abd  reman-windows-x64.zip\n", &sig, &pk));
        assert!(!signed(msg, &sig, &ed25519_dalek::SigningKey::from_bytes(&[8u8; 32]).verifying_key().to_bytes()));
        assert!(!signed(msg, &sig[..63], &pk));
        assert!(!signed(msg, &[], &pk));
        // the built-in key is a real one, not the test's
        assert_ne!(RELEASE_KEY, pk);
        assert!(ed25519_dalek::VerifyingKey::from_bytes(&RELEASE_KEY).is_ok());
    }

    #[test]
    fn addresses_are_built_not_taken() {
        let r = Release { version: "0.3.4".into() };
        if test_server().is_none() {
            assert_eq!(r.file("SHA256SUMS.txt"), "https://github.com/rdarshan10/reman/releases/download/v0.3.4/SHA256SUMS.txt");
        }
        assert_eq!(r.page(), "https://github.com/rdarshan10/reman/releases/tag/v0.3.4");
    }

    #[test]
    fn the_test_server_is_this_machine_only() {
        assert!(on_this_machine("http://127.0.0.1:8080") && on_this_machine("http://localhost:9/x"));
        for far in ["http://localhost:@evil.example/", "http://127.0.0.1.nip.io:80", "http://127.0.0.1:80@evil.example", "https://evil.example/http://127.0.0.1:1", "http://localhost", "http://127.0.0.1:", "http://localhost:123456"] {
            assert!(!on_this_machine(far), "{far}");
        }
    }

    #[test]
    fn sums_find_their_file() {
        let sums = "ABC123  reman-windows-x64.zip\ndef456 *reman-linux-x64.tar.gz\n";
        assert_eq!(expected_sum(sums, "reman-windows-x64.zip").as_deref(), Some("abc123"));
        assert_eq!(expected_sum(sums, "reman-linux-x64.tar.gz").as_deref(), Some("def456"));
        assert_eq!(expected_sum(sums, "reman-macos-arm64.tar.gz"), None);
    }
}
