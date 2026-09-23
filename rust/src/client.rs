//! Thin daemon client: one persistent localhost connection, many JSON-line requests.
//! Autostarts a detached, windowless daemon when none is listening.
use crate::config;
use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

pub struct Client {
    w: TcpStream,
    r: BufReader<TcpStream>,
}

fn addr() -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], config::port()))
}

impl Client {
    pub fn connect_timeout(t: Duration) -> Result<Self> {
        let s = TcpStream::connect_timeout(&addr(), t)?;
        s.set_nodelay(true)?;
        Ok(Self { w: s.try_clone()?, r: BufReader::new(s) })
    }

    /// Connect, starting the daemon if needed (first start loads the model: ~1s).
    pub fn connect() -> Result<Self> {
        Self::connect_with(&std::env::current_exe()?)
    }

    /// Like `connect`, but autostarts the daemon from a specific executable.
    pub fn connect_with(exe: &std::path::Path) -> Result<Self> {
        if let Ok(c) = Self::connect_timeout(Duration::from_millis(300)) {
            return Ok(c);
        }
        spawn_daemon_exe(exe)?;
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(60) {
            std::thread::sleep(Duration::from_millis(150));
            if let Ok(c) = Self::connect_timeout(Duration::from_millis(300)) {
                return Ok(c);
            }
        }
        Err(anyhow!("daemon did not come up; see {}", config::log_path().display()))
    }

    pub fn set_timeout(&self, t: Option<Duration>) {
        let _ = self.w.set_read_timeout(t);
    }

    pub fn call(&mut self, req: &Value) -> Result<Value> {
        let mut b = serde_json::to_vec(req)?;
        b.push(b'\n');
        self.w.write_all(&b)?;
        self.w.flush()?;
        let mut line = String::new();
        if self.r.read_line(&mut line)? == 0 {
            return Err(anyhow!("daemon closed the connection"));
        }
        let v: Value = serde_json::from_str(&line).context("bad daemon response")?;
        if let Some(e) = v.get("error").and_then(Value::as_str) {
            return Err(anyhow!("daemon: {e}"));
        }
        Ok(v)
    }

    /// Fire-and-forget: write the request, don't wait for the reply.
    pub fn send(&mut self, req: &Value) -> Result<()> {
        let mut b = serde_json::to_vec(req)?;
        b.push(b'\n');
        self.w.write_all(&b)?;
        Ok(self.w.flush()?)
    }
}

/// One-shot convenience.
pub fn call(req: &Value) -> Result<Value> {
    Client::connect()?.call(req)
}

pub fn daemon_up() -> bool {
    TcpStream::connect_timeout(&addr(), Duration::from_millis(200)).is_ok()
}

/// Windows hands EVERY inheritable handle to a child. If our stdout is a pipe someone is reading
/// (a hook runner, `$(...)`, an MCP client, `Command::output()`), the long-lived daemon would keep
/// that pipe open and the reader would wait for EOF forever. Make our std handles non-inheritable
/// first (std re-duplicates any handle it passes explicitly, so this is safe).
#[cfg(windows)]
fn stop_std_handle_inheritance() {
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetStdHandle(n: u32) -> *mut c_void;
        fn SetHandleInformation(h: *mut c_void, mask: u32, flags: u32) -> i32;
    }
    const HANDLE_FLAG_INHERIT: u32 = 1;
    for n in [-10i32, -11, -12] {
        unsafe {
            let h = GetStdHandle(n as u32);
            if !h.is_null() && h as isize != -1 {
                SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

/// Start `reman daemon` (from this executable) detached, with no console window.
pub fn spawn_daemon() -> Result<()> {
    spawn_daemon_exe(&std::env::current_exe()?)
}

pub fn spawn_daemon_exe(exe: &std::path::Path) -> Result<()> {
    #[cfg(windows)]
    stop_std_handle_inheritance();
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("daemon").stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        // escape the caller's job object when allowed (editors/agents kill their job on exit)
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB);
        if cmd.spawn().is_ok() {
            return Ok(());
        }
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc_setsid();
                Ok(())
            });
        }
    }
    cmd.spawn().context("spawning daemon")?;
    Ok(())
}

#[cfg(unix)]
fn libc_setsid() {
    unsafe extern "C" {
        fn setsid() -> i32;
    }
    unsafe {
        setsid();
    }
}
