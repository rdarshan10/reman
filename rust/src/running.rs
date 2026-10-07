//! What is running right now: the servers listening on a port, the folder each runs in, and what
//! started it (an agent, the user in a terminal, an editor, an app). An agent that knows the
//! user's dev server is up leaves it alone, or uses it, instead of killing it or starting another.
//! Read-only: nothing here ever stops a process.
use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

/// One process in a listener's line of ancestry.
#[derive(Debug, Clone)]
pub struct Proc {
    pub pid: u32,
    /// lowercased, without `.exe`
    pub name: String,
    pub exe: String,
    pub cmd: String,
    pub cwd: Option<String>,
    pub started: i64,
}

/// A process listening on TCP ports, with its ancestors (nearest first).
#[derive(Debug)]
pub struct Listener {
    pub ports: Vec<u16>,
    pub proc: Proc,
    pub ancestors: Vec<Proc>,
}

impl Listener {
    /// The folder it runs in: its own, else the nearest ancestor's below what started it (its own
    /// can be unreadable).
    pub fn folder(&self) -> Option<&str> {
        // never the starter's own (an agent's or a terminal's folder says nothing of the server)
        std::iter::once(&self.proc).chain(self.ancestors.iter().take_while(|a| starter_of(a).is_none())).find_map(|p| p.cwd.as_deref())
    }
}

/// What started a process, from its ancestors: the nearest one that says.
#[derive(Debug, Clone, PartialEq)]
pub enum Starter {
    /// a coding agent's CLI (`Claude Code`, `Codex`), and its pid
    Agent(&'static str, u32),
    /// an editor: the user in its terminal, or its built-in agent
    Editor(&'static str),
    /// a desktop app that runs servers itself (the Claude app's preview pane)
    App(&'static str),
    /// the user, in a terminal
    Terminal,
    Unknown,
}

impl Starter {
    pub fn kind(&self) -> &'static str {
        match self {
            Starter::Agent(..) => "agent",
            Starter::Editor(_) => "editor",
            Starter::App(_) => "app",
            Starter::Terminal => "user",
            Starter::Unknown => "unknown",
        }
    }

    pub fn label(&self) -> String {
        match self {
            Starter::Agent(a, _) => format!("an agent ({a})"),
            Starter::Editor(e) => format!("{e}: the user in its terminal, or its agent"),
            Starter::App(a) => a.to_string(),
            Starter::Terminal => "the user, in a terminal".into(),
            Starter::Unknown => "unknown (what started it has exited)".into(),
        }
    }
}

/// Classify one ancestor, or None to keep walking up.
fn starter_of(p: &Proc) -> Option<Starter> {
    let cmd = p.cmd.to_ascii_lowercase().replace('\\', "/");
    let exe = p.exe.to_ascii_lowercase().replace('\\', "/");
    let has = |s: &str| cmd.contains(s) || exe.contains(s);
    let agent = |a| Some(Starter::Agent(a, p.pid));
    match p.name.as_str() {
        _ if has("claude-code") || (p.name == "claude" && !exe.contains("windowsapps") && !exe.contains("/claude.app/")) => agent("Claude Code"),
        "codex" => agent("Codex"),
        _ if has("@openai/codex") => agent("Codex"),
        "gemini" => agent("Gemini CLI"),
        _ if has("@google/gemini-cli") => agent("Gemini CLI"),
        "opencode" => agent("opencode"),
        "cursor-agent" => agent("Cursor agent"),
        "aider" => agent("aider"),
        "goose" => agent("goose"),
        "amp" => agent("Amp"),
        _ if has("@github/copilot") => agent("Copilot CLI"),
        "claude" => Some(Starter::App("the Claude app (its preview pane or terminal)")),
        "code" | "code - insiders" | "code-insiders" => Some(Starter::Editor("VS Code")),
        "cursor" => Some(Starter::Editor("Cursor")),
        "windsurf" => Some(Starter::Editor("Windsurf")),
        "zed" => Some(Starter::Editor("Zed")),
        "idea64" | "idea" | "pycharm64" | "pycharm" | "webstorm64" | "webstorm" | "rustrover64" | "goland64" | "clion64" => Some(Starter::Editor("a JetBrains IDE")),
        "windowsterminal" | "openconsole" | "conhost" | "explorer" | "mintty" | "wezterm-gui" | "wezterm" | "alacritty" | "kitty" | "ghostty"
        | "iterm2" | "terminal" | "gnome-terminal-server" | "konsole" | "xterm" | "tabby" | "hyper" | "warp" | "tmux: server" | "tmux"
        | "screen" | "sshd" | "login" => Some(Starter::Terminal),
        _ => None,
    }
}

pub fn starter(l: &Listener) -> Starter {
    l.ancestors.iter().find_map(starter_of).unwrap_or(Starter::Unknown)
}

/// The command lines of a listener and its ancestors up to what started it, as words: what was
/// typed is in there (`cmd.exe /C npm.cmd run dev` above `node astro.js dev`). Split once, then
/// matched against every recorded command.
pub struct Lines {
    lines: Vec<Vec<String>>,
    seen: HashSet<String>,
}

impl Lines {
    pub fn of(l: &Listener) -> Self {
        let mut raw = vec![l.proc.cmd.as_str()];
        for a in &l.ancestors {
            if starter_of(a).is_some() {
                break;
            }
            raw.push(a.cmd.as_str());
        }
        Self::from_lines(&raw)
    }

    fn from_lines(raw: &[&str]) -> Self {
        let lines: Vec<Vec<String>> = raw.iter().map(|l| words(l)).collect();
        let seen = lines.iter().flatten().cloned().collect();
        Self { lines, seen }
    }

    /// Did one of these lines run `recorded`? Its words, in order, appear in the line, programs
    /// by their name (`C:\nvm4w\nodejs\npm.cmd` is `npm`); `npx astro dev` also matches `astro dev`.
    pub fn ran(&self, recorded: &str) -> bool {
        // most recorded commands share no word with these lines: rejected before any work
        let Some(first) = recorded.split_whitespace().next() else { return false };
        let first = word(first);
        let wrapper = matches!(first.as_str(), "npx" | "bunx" | "pnpx");
        if !wrapper && !self.seen.contains(&first) {
            return false;
        }
        let want = words(recorded);
        let tail = if wrapper { &want[1..] } else { &want[..] };
        let sub = |w: &[String]| !w.is_empty() && self.lines.iter().any(|have| have.windows(w.len()).any(|h| h == w));
        sub(&want) || (wrapper && tail.len() >= 2 && sub(tail))
    }
}

fn words(s: &str) -> Vec<String> {
    s.split_whitespace().map(word).filter(|w| !w.is_empty()).collect()
}

/// One word as a program name would be compared: quotes off, a path down to its file's name,
/// lowercased, without `.exe` / `.cmd` / `.js` and the like.
fn word(t: &str) -> String {
    let t = t.trim_matches(|c| c == '"' || c == '\'').replace('\\', "/");
    let base = if t.contains('/') && !t.starts_with('-') { t.rsplit('/').next().unwrap_or("").to_string() } else { t };
    let l = base.to_ascii_lowercase();
    [".exe", ".cmd", ".bat", ".ps1", ".js", ".mjs", ".cjs"].iter().find_map(|x| l.strip_suffix(x)).map(str::to_string).unwrap_or(l)
}

/// Every process listening on a TCP port, with its ancestors. Empty when the OS won't say.
pub fn listeners() -> Vec<Listener> {
    let ports = listening_ports();
    if ports.is_empty() {
        return Vec::new();
    }
    let mut sys = tree();
    let me = std::process::id();
    // the System process (4 on Windows) and pid 0 listen for the OS, never for a project
    let chains: Vec<(u32, Vec<u32>)> = ports.keys().filter(|&&p| p != me && p > 4).map(|&p| (p, chain(&sys, p))).collect();
    let need: Vec<u32> = chains.iter().flat_map(|(_, c)| c.iter().copied()).collect();
    details(&mut sys, &need);
    chains
        .into_iter()
        .filter_map(|(pid, c)| {
            let proc = proc_of(&sys, pid)?;
            let ancestors = c[1..].iter().filter_map(|&a| proc_of(&sys, a)).collect();
            Some(Listener { ports: ports[&pid].clone(), proc, ancestors })
        })
        .collect()
}

/// Every process with its name and parent only: the cheap part. Command lines, folders and
/// programs are read later for the few processes that matter (`details`).
fn tree() -> System {
    let mut sys = System::new();
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
    sys
}

/// `pid` and its ancestors, nearest first (a parent that exited ends it).
fn chain(sys: &System, pid: u32) -> Vec<u32> {
    let mut out = vec![pid];
    let mut cur = Pid::from_u32(pid);
    while let Some(parent) = sys.process(cur).and_then(|p| p.parent()) {
        // a pid reused by a process younger than its "child" is not its parent
        let younger = match (sys.process(parent), sys.process(cur)) {
            (Some(p), Some(c)) => p.start_time() > c.start_time(),
            _ => true,
        };
        if younger || out.len() > 24 || out.contains(&parent.as_u32()) {
            break;
        }
        out.push(parent.as_u32());
        cur = parent;
    }
    out
}

fn details(sys: &mut System, pids: &[u32]) {
    let mut need: Vec<Pid> = pids.iter().map(|&p| Pid::from_u32(p)).collect();
    need.sort();
    need.dedup();
    let kind = ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always).with_cwd(UpdateKind::Always).with_exe(UpdateKind::Always);
    sys.refresh_processes_specifics(ProcessesToUpdate::Some(&need), false, kind);
}

fn proc_of(sys: &System, pid: u32) -> Option<Proc> {
    let p = sys.process(Pid::from_u32(pid))?;
    let name = p.name().to_string_lossy().to_ascii_lowercase();
    Some(Proc {
        pid,
        name: name.strip_suffix(".exe").map(str::to_string).unwrap_or(name),
        exe: p.exe().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default(),
        cmd: p.cmd().iter().map(|a| a.to_string_lossy()).collect::<Vec<_>>().join(" "),
        cwd: p.cwd().map(|c| c.to_string_lossy().into_owned()).filter(|c| !c.is_empty()),
        started: p.start_time() as i64,
    })
}

/// pid -> the TCP ports it listens on
fn listening_ports() -> BTreeMap<u32, Vec<u16>> {
    let mut out: BTreeMap<u32, Vec<u16>> = BTreeMap::new();
    for (pid, port) in raw_listening() {
        let v = out.entry(pid).or_default();
        if !v.contains(&port) {
            v.push(port);
        }
    }
    for v in out.values_mut() {
        v.sort();
    }
    out
}

/// What a program prints, or None when it can't run or takes longer than `limit` (then it is
/// stopped): an agent's call never hangs on a stuck system tool.
#[cfg_attr(target_os = "linux", allow(dead_code))]
fn output_within(prog: &str, args: &[&str], limit: Duration) -> Option<String> {
    use std::io::Read;
    let mut c = std::process::Command::new(prog);
    c.args(args).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // no console window
    }
    let mut child = c.spawn().ok()?;
    let mut out = child.stdout.take()?;
    // read on a thread of its own, so a full pipe can't block the wait
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        out.read_to_end(&mut buf).ok().map(|_| buf)
    });
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(5)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    reader.join().ok().flatten().map(|b| String::from_utf8_lossy(&b).into_owned())
}

#[cfg_attr(target_os = "linux", allow(dead_code))]
const TOOL_LIMIT: Duration = Duration::from_secs(3);

#[cfg(windows)]
fn raw_listening() -> Vec<(u32, u16)> {
    // IPv4 and IPv6 side by side: one netstat's time, not two
    let v6 = std::thread::spawn(|| output_within("netstat", &["-ano", "-p", "TCPv6"], TOOL_LIMIT));
    let mut out = output_within("netstat", &["-ano", "-p", "TCP"], TOOL_LIMIT).map(|t| parse_netstat(&t)).unwrap_or_default();
    out.extend(v6.join().ok().flatten().map(|t| parse_netstat(&t)).unwrap_or_default());
    out
}

/// `netstat -ano` lines that listen: the remote end is `0.0.0.0:0` or `[::]:0` (the state's word
/// is translated on some Windows, the address isn't)
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_netstat(text: &str) -> Vec<(u32, u16)> {
    text.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            if f.len() < 4 || !f[0].starts_with("TCP") || !matches!(f[2], "0.0.0.0:0" | "[::]:0") {
                return None;
            }
            let port = f[1].rsplit(':').next()?.parse().ok()?;
            let pid = f.last()?.parse().ok()?;
            Some((pid, port))
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn raw_listening() -> Vec<(u32, u16)> {
    // listening sockets by inode, then which process holds each
    let mut by_inode: std::collections::HashMap<String, u16> = std::collections::HashMap::new();
    for f in ["/proc/net/tcp", "/proc/net/tcp6"] {
        for l in std::fs::read_to_string(f).unwrap_or_default().lines().skip(1) {
            let c: Vec<&str> = l.split_whitespace().collect();
            if c.len() > 9 && c[3] == "0A" {
                if let Some(port) = c[1].rsplit(':').next().and_then(|p| u16::from_str_radix(p, 16).ok()) {
                    by_inode.insert(c[9].to_string(), port);
                }
            }
        }
    }
    if by_inode.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for d in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = d.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        for fd in std::fs::read_dir(d.path().join("fd")).into_iter().flatten().flatten() {
            if let Ok(t) = std::fs::read_link(fd.path()) {
                let t = t.to_string_lossy();
                if let Some(port) = t.strip_prefix("socket:[").and_then(|r| r.strip_suffix(']')).and_then(|i| by_inode.get(i)) {
                    out.push((pid, *port));
                }
            }
        }
    }
    out
}

#[cfg(all(unix, not(target_os = "linux")))]
fn raw_listening() -> Vec<(u32, u16)> {
    let Some(text) = output_within("lsof", &["-nP", "-iTCP", "-sTCP:LISTEN", "-Fpn"], TOOL_LIMIT) else { return Vec::new() };
    let mut out = Vec::new();
    let mut pid = 0;
    for l in text.lines() {
        if let Some(p) = l.strip_prefix('p') {
            pid = p.parse().unwrap_or(0);
        } else if let Some(port) = l.strip_prefix('n').and_then(|n| n.rsplit(':').next()).and_then(|p| p.parse().ok()) {
            if pid > 0 {
                out.push((pid, port));
            }
        }
    }
    out
}

/// The processes the agent asking runs under (its bridge's parent, and the agent CLI above it):
/// a listener under one of them was started by this very session.
pub fn session_pids() -> Vec<u32> {
    let mut sys = tree();
    let up = chain(&sys, std::process::id());
    let up = &up[1..up.len().min(13)];
    details(&mut sys, up);
    let mut out: Vec<u32> = up.first().copied().into_iter().collect();
    for &pid in up {
        let Some(p) = proc_of(&sys, pid) else { break };
        match starter_of(&p) {
            Some(Starter::Agent(..)) => {
                if !out.contains(&pid) {
                    out.push(pid);
                }
                break;
            }
            Some(_) => break,
            None => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: u32, name: &str, exe: &str, cmd: &str) -> Proc {
        Proc { pid, name: name.into(), exe: exe.into(), cmd: cmd.into(), cwd: None, started: 0 }
    }

    #[test]
    fn netstat_lines_that_listen() {
        let t = "\nActive Connections\n\n  Proto  Local Address          Foreign Address        State           PID\n  TCP    0.0.0.0:4321           0.0.0.0:0              LISTENING       31972\n  TCP    127.0.0.1:4400         0.0.0.0:0              ABHÖREN         812\n  TCP    127.0.0.1:50000        127.0.0.1:4321         ESTABLISHED     900\n  TCP    [::]:5173              [::]:0                 LISTENING       77\n";
        assert_eq!(parse_netstat(t), vec![(31972, 4321), (812, 4400), (77, 5173)]);
    }

    #[test]
    fn what_started_it() {
        let node = p(1, "node", r"C:\nvm4w\nodejs\node.exe", r"node C:\site\node_modules\astro\astro.js dev");
        let npm = p(2, "cmd", r"C:\WINDOWS\System32\cmd.exe", r"C:\WINDOWS\System32\cmd.exe /C C:\nvm4w\nodejs\npm.cmd run dev");
        let app = p(3, "claude", r"C:\Program Files\WindowsApps\Claude_2.1_x64__x\app\Claude.exe", "Claude.exe");
        let code = p(4, "claude", r"C:\Users\me\AppData\Roaming\Claude\claude-code\2.1.286\x\claude.exe", "claude.exe --output-format stream-json");
        let wt = p(5, "windowsterminal", r"C:\x\WindowsTerminal.exe", "wt");
        let pwsh = p(6, "pwsh", r"C:\x\pwsh.exe", "pwsh");
        let l = |a: Vec<Proc>| Listener { ports: vec![4321], proc: node.clone(), ancestors: a };
        assert_eq!(starter(&l(vec![npm.clone(), app.clone()])), Starter::App("the Claude app (its preview pane or terminal)"));
        assert_eq!(starter(&l(vec![npm.clone(), code.clone(), app.clone()])), Starter::Agent("Claude Code", 4));
        assert_eq!(starter(&l(vec![npm.clone(), pwsh.clone(), wt.clone()])), Starter::Terminal);
        assert_eq!(starter(&l(vec![npm.clone()])), Starter::Unknown);
        // what was typed is in the lines up to the starter
        let ls = Lines::of(&l(vec![npm.clone(), pwsh, wt]));
        assert!(ls.ran("npm run dev"));
        assert!(ls.ran("npx astro dev"));
        assert!(!ls.ran("npm run build"));
        // the lines stop at what started it: the terminal's own command line is not searched
        assert!(!ls.ran("wt"));
    }

    fn ran(line: &str, recorded: &str) -> bool {
        Lines::from_lines(&[line]).ran(recorded)
    }

    #[test]
    fn a_recorded_command_in_a_process_line() {
        assert!(ran(r"C:\WINDOWS\system32\cmd.exe /d /s /c astro dev", "astro dev"));
        assert!(ran(r#"python -m uvicorn app.main:app --reload --port 8000"#, "python -m uvicorn app.main:app --reload --port 8000"));
        assert!(!ran("node server.js", "npm run dev"));
        assert!(!ran("node astro.js dev", "npx"));
        assert!(!ran("node astro.js dev", ""));
        assert!(ran(r"C:\nvm4w\nodejs\npm.cmd run dev", "npm run dev"));
    }

    #[test]
    fn a_stuck_tool_is_stopped() {
        let (prog, args): (&str, &[&str]) = if cfg!(windows) { ("powershell", &["-NoProfile", "-Command", "Start-Sleep 10"]) } else { ("sleep", &["10"]) };
        let t = Instant::now();
        assert!(output_within(prog, args, Duration::from_millis(300)).is_none());
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn listeners_are_found_quickly() {
        // a port of our own, then the real lookup: it must find it, with this process as the
        // listener and its ancestors read, and not take long
        let sock = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = sock.local_addr().unwrap().port();
        let t = Instant::now();
        let ls = super::listening_ports();
        let took = t.elapsed();
        assert!(ls.get(&std::process::id()).is_some_and(|p| p.contains(&port)), "{ls:?}");
        assert!(took < Duration::from_secs(2), "{took:?}");
        let t = Instant::now();
        let mut sys = tree();
        let c = chain(&sys, std::process::id());
        details(&mut sys, &c);
        assert!(proc_of(&sys, std::process::id()).is_some_and(|p| p.cwd.is_some() && !p.cmd.is_empty()));
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    }
}
