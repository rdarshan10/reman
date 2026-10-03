//! `reman shell`: your shell inside a terminal of reman's own (ConPTY on Windows, a pseudo-terminal
//! elsewhere), passed through untouched, so what each command prints can be kept. The shell
//! integration marks where a command's output starts and ends (an OSC private to reman); between
//! the marks the screen is read the way a terminal draws it (vt100: cursor moves, progress bars and
//! redraws come out as the final text), and the text goes to the daemon with the run's id.
//! The design follows Atuin's pty-proxy (MIT License, Copyright (c) 2021 Ellie Huxtable; see
//! THIRD_PARTY_NOTICES.md), here on Windows too.
use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::io::{Read, Write};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How a start or end mark begins: `ESC ] 46031711 ;` then `C;<id>` or `D;<id>`, then BEL.
/// (46031711 is `reman` read as a base-36 number: an OSC number no terminal uses.)
const MARK: &[u8] = b"\x1b]46031711;";
/// Lines kept above the screen while a command runs (its output's start, when it's long).
const SCROLLBACK: usize = 4000;
/// The exit code when the shell couldn't be started at all (the profile then goes on as usual).
pub const NOT_STARTED: i32 = 97;

/// What reman shell saw (the marks, what each command printed, every byte with REMAN_PTY_LOG_BYTES),
/// written to the file REMAN_PTY_LOG names: for telling why a command's output wasn't kept.
fn log(what: impl FnOnce() -> String) {
    static FILE: std::sync::OnceLock<Option<Mutex<std::fs::File>>> = std::sync::OnceLock::new();
    let f = FILE.get_or_init(|| std::env::var_os("REMAN_PTY_LOG").and_then(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).ok()).map(Mutex::new));
    if let Some(f) = f {
        if let Ok(mut f) = f.lock() {
            let _ = writeln!(f, "{}", what());
        }
    }
}

/// The shell to run when none is named: the one this was started from, as far as can be told.
fn default_shell() -> Vec<String> {
    if let Ok(s) = std::env::var("REMAN_SHELL") {
        return s.split_whitespace().map(String::from).collect();
    }
    if cfg!(windows) {
        // PowerShell 7 sets this for its children; Windows PowerShell doesn't
        let pwsh = std::env::var("PSModulePath").is_ok_and(|p| p.to_lowercase().contains("powershell\\7"));
        vec![if pwsh { "pwsh.exe" } else { "powershell.exe" }.into(), "-NoLogo".into()]
    } else {
        vec![std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())]
    }
}

pub fn run(program: Vec<String>) -> Result<i32> {
    if std::env::var_os("REMAN_PTY").is_some() {
        eprintln!("reman: this shell already runs inside `reman shell`");
        return Ok(0);
    }
    let program = if program.is_empty() { default_shell() } else { program };
    let (cols, rows) = ratatui::crossterm::terminal::size().unwrap_or((120, 30));
    let pair = match native_pty_system().openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("reman shell: no pseudo-terminal here ({e}); running the shell as it is");
            return Ok(NOT_STARTED);
        }
    };
    let mut cmd = CommandBuilder::new(&program[0]);
    cmd.args(&program[1..]);
    if let Ok(d) = std::env::current_dir() {
        cmd.cwd(d);
    }
    cmd.env("REMAN_PTY", "1");
    let mut child = match pair.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("reman shell: could not start {} ({e})", program[0]);
            return Ok(NOT_STARTED);
        }
    };
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().context("reading the shell's terminal")?;
    let writer = Arc::new(Mutex::new(pair.master.take_writer().context("writing to the shell's terminal")?));
    let master = Arc::new(Mutex::new(Some(pair.master)));

    let console = Console::raw()?;
    // keys: everything typed goes to the shell as it is
    let keys = writer.clone();
    std::thread::spawn(move || {
        let mut input = console_input();
        let mut buf = [0u8; 4096];
        while let Ok(n) = input.read(&mut buf) {
            if std::env::var_os("REMAN_PTY_LOG_BYTES").is_some() {
                log(|| format!("keys {:?}", String::from_utf8_lossy(&buf[..n])));
            }
            let Ok(mut w) = keys.lock() else { break };
            if n == 0 || w.write_all(&buf[..n]).is_err() {
                break;
            }
            let _ = w.flush();
        }
    });
    // the window's size, followed
    let (size_tx, size_rx) = channel::<(u16, u16)>();
    {
        let master = master.clone();
        std::thread::spawn(move || {
            let mut last = (cols, rows);
            loop {
                std::thread::sleep(Duration::from_millis(200));
                let Ok(now) = ratatui::crossterm::terminal::size() else { continue };
                if now != last {
                    last = now;
                    match master.lock().ok().and_then(|m| m.as_ref().map(|m| m.resize(PtySize { rows: now.1, cols: now.0, pixel_width: 0, pixel_height: 0 }))) {
                        Some(_) => {
                            let _ = size_tx.send(now);
                        }
                        None => break,
                    }
                }
            }
        });
    }
    // what each command printed, to the daemon (never in the way of the screen)
    let (out_tx, out_rx) = channel::<(String, String)>();
    std::thread::spawn(move || {
        for (id, text) in out_rx {
            let _ = crate::client::call(&serde_json::json!({"op": "output", "capture": id, "text": text}));
        }
    });
    // the screen: everything the shell draws, drawn
    let pump = std::thread::spawn(move || {
        let mut out = console_output();
        let mut cap = Capture::new(rows, cols, out_tx);
        let mut carry: Vec<u8> = Vec::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            while let Ok((c, r)) = size_rx.try_recv() {
                cap.resize(r, c);
            }
            let shown = cap.push(&buf[..n]);
            // a character split across two reads is written whole, with the next
            #[cfg(windows)]
            let shown = answer_cursor_queries(shown, &mut carry, &mut out, &writer);
            carry.extend_from_slice(&shown);
            let whole = utf8_prefix(&carry);
            if out.write_all(&carry[..whole]).is_err() {
                break;
            }
            let _ = out.flush();
            carry.drain(..whole);
        }
    });
    let status = child.wait();
    // ConPTY keeps its output open until it's closed: close it, so the screen pump ends
    if let Ok(mut m) = master.lock() {
        m.take();
    }
    let _ = pump.join();
    drop(console);
    Ok(status.map(|s| s.exit_code() as i32).unwrap_or(1))
}

/// ConPTY asks where the cursor is when it starts (`ESC [ 6 n`) and waits for the answer before
/// anything is shown; a terminal that never answers would leave the shell unborn. On Windows the
/// console itself knows: the query is answered from it, here, and not passed on (a terminal that
/// answers too would leave its reply typed into the shell). Returns the bytes left to show.
#[cfg(windows)]
fn answer_cursor_queries(shown: Vec<u8>, carry: &mut Vec<u8>, out: &mut Box<dyn Write + Send>, writer: &Mutex<Box<dyn Write + Send>>) -> Vec<u8> {
    const Q: &[u8] = b"\x1b[6n";
    let mut rest = shown;
    while let Some(i) = find(&rest, Q) {
        // what came before it is drawn first, so the position is where the query was asked
        carry.extend_from_slice(&rest[..i]);
        let _ = out.write_all(carry);
        let _ = out.flush();
        carry.clear();
        let pos = ratatui::crossterm::cursor::position();
        log(|| format!("cursor query: {pos:?}"));
        let (x, y) = pos.unwrap_or((0, 0));
        if let Ok(mut w) = writer.lock() {
            // one write: in pieces, ConPTY would read the ESC alone as the Escape key
            let reply = format!("\x1b[{};{}R", y + 1, x + 1);
            let _ = w.write_all(reply.as_bytes());
            let _ = w.flush();
        }
        rest.drain(..i + Q.len());
    }
    rest
}

/// The bytes of `b` up to its last complete UTF-8 character (a lead byte at the end waits).
fn utf8_prefix(b: &[u8]) -> usize {
    let n = b.len();
    for back in 1..=3.min(n) {
        let c = b[n - back];
        if c & 0xC0 != 0x80 {
            // a lead byte (or ASCII): complete when the sequence it starts fits
            let need = if c >= 0xF0 { 4 } else if c >= 0xE0 { 3 } else if c >= 0xC0 { 2 } else { 1 };
            return if need > back { n - back } else { n };
        }
    }
    n
}

/// The marks in the shell's output, and the screen as it is between them.
struct Capture {
    rows: u16,
    cols: u16,
    /// the command being run: its id, and the screen since its start mark
    open: Option<(String, vt100::Parser)>,
    /// a mark cut in two by a read: its first part, until the rest comes
    pending: Vec<u8>,
    tx: Sender<(String, String)>,
}

impl Capture {
    fn new(rows: u16, cols: u16, tx: Sender<(String, String)>) -> Self {
        Capture { rows, cols, open: None, pending: Vec::new(), tx }
    }

    fn resize(&mut self, rows: u16, cols: u16) {
        (self.rows, self.cols) = (rows, cols);
        if let Some((_, p)) = self.open.as_mut() {
            p.screen_mut().set_size(rows, cols);
        }
    }

    /// The bytes to show (the marks taken out), noting what's between them.
    fn push(&mut self, data: &[u8]) -> Vec<u8> {
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(data);
        let mut shown = Vec::with_capacity(buf.len());
        let mut at = 0;
        while let Some(i) = find(&buf[at..], MARK).map(|i| i + at) {
            self.screen(&buf[at..i], &mut shown);
            let body = i + MARK.len();
            // the mark ends with BEL or ESC \
            let Some(end) = buf[body..].iter().position(|&b| b == 0x07 || b == 0x1b).map(|e| e + body) else {
                self.pending = buf[i..].to_vec();
                return shown;
            };
            let close = if buf[end] == 0x1b { 2 } else { 1 };
            if close == 2 && end + 1 >= buf.len() {
                self.pending = buf[i..].to_vec();
                return shown;
            }
            self.mark(&String::from_utf8_lossy(&buf[body..end]));
            at = end + close;
        }
        // the end may be the start of a mark
        let rest = &buf[at..];
        let keep = (1..MARK.len().min(rest.len() + 1)).rev().find(|&k| rest.ends_with(&MARK[..k])).unwrap_or(0);
        self.screen(&rest[..rest.len() - keep], &mut shown);
        self.pending = rest[rest.len() - keep..].to_vec();
        shown
    }

    fn screen(&mut self, bytes: &[u8], shown: &mut Vec<u8>) {
        if std::env::var_os("REMAN_PTY_LOG_BYTES").is_some() {
            log(|| format!("bytes {:?}", String::from_utf8_lossy(bytes)));
        }
        shown.extend_from_slice(bytes);
        if let Some((_, p)) = self.open.as_mut() {
            p.process(bytes);
        }
    }

    fn mark(&mut self, params: &str) {
        log(|| format!("mark {params}"));
        match params.split_once(';') {
            // a command starts: a blank screen of our own from here (the real one is untouched)
            Some(("C", id)) => self.open = Some((id.to_string(), vt100::Parser::new(self.rows, self.cols, SCROLLBACK))),
            // it ended: what's on that screen is what it printed
            Some(("D", id)) => {
                if let Some((open, mut p)) = self.open.take() {
                    let text = screen_text(&mut p);
                    log(|| format!("printed by {open}: {text:?}"));
                    // nothing printed, nothing to keep
                    if open == id && !text.trim().is_empty() {
                        let _ = self.tx.send((open, text));
                    }
                }
            }
            _ => {}
        }
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Everything on a screen, the lines that scrolled off it first; a line the terminal wrapped is
/// one line again.
fn screen_text(p: &mut vt100::Parser) -> String {
    let screen = p.screen_mut();
    let (rows, cols) = screen.size();
    screen.set_scrollback(usize::MAX);
    let above = screen.scrollback();
    let mut out = String::new();
    let take = |screen: &vt100::Screen, n: usize, out: &mut String| {
        for (i, text) in screen.rows(0, cols).take(n).enumerate() {
            out.push_str(text.trim_end());
            if !screen.row_wrapped(i as u16) {
                out.push('\n');
            }
        }
    };
    let mut off = above;
    while off > 0 {
        screen.set_scrollback(off);
        let n = off.min(rows as usize);
        take(screen, n, &mut out);
        off -= n;
    }
    screen.set_scrollback(0);
    take(screen, rows as usize, &mut out);
    out.trim_end().to_string()
}

/// The terminal reman shell runs in: keys passed as typed, output drawn as sent, put back on drop.
struct Console {
    #[cfg(windows)]
    saved: (u32, u32, u32, u32),
}

#[cfg(windows)]
mod win {
    pub const STD_INPUT: u32 = -10i32 as u32;
    pub const STD_OUTPUT: u32 = -11i32 as u32;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub fn GetStdHandle(n: u32) -> *mut core::ffi::c_void;
        pub fn GetConsoleMode(h: *mut core::ffi::c_void, mode: *mut u32) -> i32;
        pub fn SetConsoleMode(h: *mut core::ffi::c_void, mode: u32) -> i32;
        pub fn GetConsoleCP() -> u32;
        pub fn SetConsoleCP(cp: u32) -> i32;
        pub fn GetConsoleOutputCP() -> u32;
        pub fn SetConsoleOutputCP(cp: u32) -> i32;
    }
}

impl Console {
    #[cfg(windows)]
    fn raw() -> Result<Self> {
        const VT_INPUT: u32 = 0x0200;
        const PROCESSED_OUTPUT: u32 = 0x0001;
        const VT_PROCESSING: u32 = 0x0004;
        const NO_AUTO_RETURN: u32 = 0x0008;
        unsafe {
            let (i, o) = (win::GetStdHandle(win::STD_INPUT), win::GetStdHandle(win::STD_OUTPUT));
            let (mut im, mut om) = (0u32, 0u32);
            win::GetConsoleMode(i, &mut im);
            win::GetConsoleMode(o, &mut om);
            let saved = (im, om, win::GetConsoleCP(), win::GetConsoleOutputCP());
            // keys as VT sequences, nothing processed or echoed here: the shell's terminal does that
            win::SetConsoleMode(i, VT_INPUT);
            win::SetConsoleMode(o, PROCESSED_OUTPUT | VT_PROCESSING | NO_AUTO_RETURN);
            win::SetConsoleCP(65001);
            win::SetConsoleOutputCP(65001);
            Ok(Console { saved })
        }
    }

    #[cfg(not(windows))]
    fn raw() -> Result<Self> {
        ratatui::crossterm::terminal::enable_raw_mode()?;
        Ok(Console {})
    }
}

impl Drop for Console {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            let (im, om, cp, ocp) = self.saved;
            win::SetConsoleMode(win::GetStdHandle(win::STD_INPUT), im);
            win::SetConsoleMode(win::GetStdHandle(win::STD_OUTPUT), om);
            win::SetConsoleCP(cp);
            win::SetConsoleOutputCP(ocp);
        }
        #[cfg(not(windows))]
        let _ = ratatui::crossterm::terminal::disable_raw_mode();
    }
}

/// Keys as bytes (on Windows the console's own input, read as UTF-8 VT sequences).
fn console_input() -> Box<dyn Read + Send> {
    #[cfg(windows)]
    if let Ok(f) = std::fs::OpenOptions::new().read(true).write(true).open("CONIN$") {
        return Box::new(f);
    }
    Box::new(std::io::stdin())
}

/// The screen, written as bytes (std's stdout would refuse a character split across two writes).
fn console_output() -> Box<dyn Write + Send> {
    #[cfg(windows)]
    if let Ok(f) = crate::tui::tty() {
        return Box::new(f);
    }
    Box::new(std::io::stdout())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap() -> (Capture, std::sync::mpsc::Receiver<(String, String)>) {
        let (tx, rx) = channel();
        (Capture::new(10, 40, tx), rx)
    }

    #[test]
    fn marks_are_taken_out_and_what_is_between_is_read() {
        let (mut c, rx) = cap();
        let shown = c.push(b"PS> npm test\r\n\x1b]46031711;C;s-1\x07\x1b[32mPASS\x1b[0m 3 tests\r\n10%\r100%\r\n\x1b]46031711;D;s-1\x07PS> ");
        assert_eq!(String::from_utf8_lossy(&shown), "PS> npm test\r\n\x1b[32mPASS\x1b[0m 3 tests\r\n10%\r100%\r\nPS> ");
        let (id, text) = rx.try_recv().unwrap();
        assert_eq!((id.as_str(), text.as_str()), ("s-1", "PASS 3 tests\n100%"));
    }

    #[test]
    fn a_mark_split_across_reads() {
        let (mut c, rx) = cap();
        let mut shown = c.push(b"x\x1b]4603");
        shown.extend(c.push(b"1711;C;a\x07out\r\n\x1b]46031711;D;"));
        shown.extend(c.push(b"a\x1b"));
        shown.extend(c.push(b"\\y"));
        assert_eq!(String::from_utf8_lossy(&shown), "xout\r\ny");
        assert_eq!(rx.try_recv().unwrap(), ("a".to_string(), "out".to_string()));
    }

    #[test]
    fn long_output_keeps_what_scrolled_off() {
        let (mut c, rx) = cap();
        c.push(b"\x1b]46031711;C;b\x07");
        for i in 0..25 {
            c.push(format!("line {i}\r\n").as_bytes());
        }
        c.push(b"\x1b]46031711;D;b\x07");
        let (_, text) = rx.try_recv().unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!((lines.first().copied(), lines.last().copied(), lines.len()), (Some("line 0"), Some("line 24"), 25));
    }

    #[test]
    fn an_end_without_its_start_says_nothing() {
        let (mut c, rx) = cap();
        c.push(b"\x1b]46031711;C;one\x07a\r\n\x1b]46031711;D;two\x07");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn utf8_written_whole() {
        let s = "a→".as_bytes();
        assert_eq!(utf8_prefix(&s[..2]), 1);
        assert_eq!(utf8_prefix(s), s.len());
    }
}
