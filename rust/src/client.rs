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
        if let Ok(c) = Self::connect_timeout(Duration::from_millis(300)) {
            return Ok(c);
        }
        spawn_daemon()?;
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

/// Start `reman daemon` detached from this shell, with no console window.
pub fn spawn_daemon() -> Result<()> {
    let exe = std::env::current_exe()?;
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
