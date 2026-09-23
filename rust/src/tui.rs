//! `reman find` - the native finder, identical on pwsh / bash / zsh / fish.
//! Draws on the terminal device (stdout stays free for the pick, so `$(reman find)` works), talks to the daemon
//! from a worker thread (typing never blocks on a request; stale searches are dropped), and only
//! ever fetches a page of results - never the whole history.
use crate::client::Client;
use anyhow::Result;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use ratatui::crossterm::{cursor, execute};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Recall,
    Fixes,
    Flows,
}

#[derive(Clone, Copy, PartialEq)]
enum ScopeSel {
    Folder,
    Repo,
    All,
}

impl ScopeSel {
    fn name(self) -> &'static str {
        match self {
            ScopeSel::Folder => "folder",
            ScopeSel::Repo => "repo",
            ScopeSel::All => "all",
        }
    }
}

#[derive(Clone, Default)]
struct Item {
    command: String,
    actor: String,
    status: String,
    predicted: bool,
    pinned: bool,
    meta: Value,
}

enum Job {
    Search(u64, Mode, Value, Value),
    Other(&'static str, Value),
}

enum Reply {
    Results(u64, Vec<Item>, usize, String),
    Detail(String, Value),
    Done(&'static str, Value),
    Error(String),
}

pub struct Opts {
    pub query: String,
    pub scope: String,
    pub cwd: String,
    pub result_file: Option<String>,
}

fn items_from(v: &Value, predicted: bool) -> Vec<Item> {
    v.get("results")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|x| Item {
                    command: x["command"].as_str().unwrap_or("").to_string(),
                    actor: x["actor"].as_str().unwrap_or("human").to_string(),
                    status: x["status"].as_str().unwrap_or("unknown").to_string(),
                    pinned: x["pinned"].as_bool().unwrap_or(false),
                    predicted,
                    meta: x.clone(),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn worker(rx: Receiver<Job>, tx: Sender<Reply>) {
    let mut client: Option<Client> = None;
    let mut call = |req: &Value| -> Result<Value> {
        if client.is_none() {
            client = Some(Client::connect()?);
        }
        let r = client.as_mut().unwrap().call(req);
        if r.is_err() {
            client = None;
        }
        r
    };
    while let Ok(first) = rx.recv() {
        // drain: run every non-search job in order, but only the NEWEST search
        let mut jobs = vec![first];
        while let Ok(j) = rx.try_recv() {
            jobs.push(j);
        }
        let last_search = jobs.iter().rposition(|j| matches!(j, Job::Search(..)));
        for (i, job) in jobs.into_iter().enumerate() {
            match job {
                Job::Search(seq, mode, req, extra) => {
                    if Some(i) != last_search {
                        continue;
                    }
                    let res = (|| -> Result<(Vec<Item>, usize, String)> {
                        let r = call(&req)?;
                        let total = r["total"].as_u64().unwrap_or(0) as usize;
                        let label = r["mode"].as_str().unwrap_or("").to_string();
                        let mut items = if mode == Mode::Flows {
                            r["results"]
                                .as_array()
                                .map(|a| {
                                    a.iter()
                                        .map(|f| {
                                            let seq: Vec<&str> = f["sequence"].as_array().map(|s| s.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                                            Item { command: seq.join(" ; "), actor: format!("{}x", f["count"]), status: "flow".into(), meta: f.clone(), ..Default::default() }
                                        })
                                        .collect()
                                })
                                .unwrap_or_default()
                        } else {
                            items_from(&r, false)
                        };
                        // empty-query recall: predicted next commands first
                        if !extra.is_null() {
                            if let Ok(p) = call(&extra) {
                                let mut pred = items_from(&p, true);
                                items.retain(|it| !pred.iter().any(|x| x.command == it.command));
                                pred.append(&mut items);
                                items = pred;
                            }
                        }
                        Ok((items, total, label))
                    })();
                    let _ = tx.send(match res {
                        Ok((items, total, label)) => Reply::Results(seq, items, total, label),
                        Err(e) => Reply::Error(e.to_string()),
                    });
                }
                Job::Other(tag, req) => {
                    let reply = match call(&req) {
                        Ok(v) if tag == "detail" => Reply::Detail(req["command"].as_str().unwrap_or("").to_string(), v),
                        Ok(v) => Reply::Done(tag, v),
                        Err(e) => Reply::Error(e.to_string()),
                    };
                    let _ = tx.send(reply);
                }
            }
        }
    }
}

struct App {
    query: String,
    mode: Mode,
    scope: ScopeSel,
    actor: &'static str,
    status: &'static str,
    group: bool,
    cwd: String,
    items: Vec<Item>,
    total: usize,
    label: String,
    sel: usize,
    want: usize,
    seq: u64,
    shown_seq: u64,
    details: HashMap<String, Value>,
    confirm_forget: Option<String>,
    message: Option<String>,
    jobs: Sender<Job>,
}

impl App {
    fn refresh(&mut self) {
        self.seq += 1;
        let scope_fields = |req: &mut Value, scope: ScopeSel, cwd: &str| {
            req["cwd"] = json!(cwd);
            req["scope"] = json!(scope.name());
        };
        let q = self.query.trim().to_string();
        let mut extra = Value::Null;
        let req = match self.mode {
            Mode::Recall => {
                let mut r = if q.is_empty() {
                    extra = json!({"op": "next", "cwd": self.cwd, "k": 3});
                    json!({"op": "recent", "k": self.want})
                } else {
                    json!({"op": "search", "query": q, "k": self.want, "group": self.group})
                };
                scope_fields(&mut r, self.scope, &self.cwd);
                if self.actor != "all" {
                    r["actor"] = json!(if self.actor == "you" { "human" } else { "agent" });
                }
                if self.status != "all" {
                    r["status"] = json!(self.status);
                }
                r
            }
            Mode::Fixes => {
                if q.is_empty() {
                    let mut r = json!({"op": "recent", "k": self.want, "status": "fail"});
                    scope_fields(&mut r, self.scope, &self.cwd);
                    r
                } else {
                    let mut r = json!({"op": "didyoumean", "query": q, "k": 30, "worked_only": true});
                    if self.scope == ScopeSel::Folder {
                        r["here"] = json!(self.cwd);
                    }
                    r
                }
            }
            Mode::Flows => {
                let mut r = json!({"op": "flows", "k": 60});
                if self.scope == ScopeSel::Folder {
                    r["cwd"] = json!(self.cwd);
                }
                r
            }
        };
        let _ = self.jobs.send(Job::Search(self.seq, self.mode, req, extra));
    }

    fn visible_items(&self) -> Vec<&Item> {
        if self.mode != Mode::Flows || self.query.trim().is_empty() {
            return self.items.iter().collect();
        }
        let toks: Vec<String> = self.query.split_whitespace().map(|t| t.to_lowercase()).collect();
        self.items.iter().filter(|it| {
            let l = it.command.to_lowercase();
            toks.iter().all(|t| l.contains(t.as_str()))
        }).collect()
    }

    fn selected(&self) -> Option<Item> {
        self.visible_items().get(self.sel).map(|x| (*x).clone())
    }

    fn reset(&mut self) {
        self.sel = 0;
        self.want = 200;
        self.confirm_forget = None;
        self.refresh();
    }
}

fn clean(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

fn fit(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n <= w { format!("{s}{}", " ".repeat(w - n)) } else if w > 1 { format!("{}…", s.chars().take(w - 1).collect::<String>()) } else { String::new() }
}

/// Split `text` into (segment, is_match) by case-insensitive query-token hits.
fn highlight(text: &str, query: &str) -> Vec<(String, bool)> {
    let chars: Vec<char> = text.chars().collect();
    let lower: Vec<char> = chars.iter().map(|c| c.to_ascii_lowercase()).collect();
    let mut mark = vec![false; chars.len()];
    for tok in query.split_whitespace().filter(|t| t.chars().count() >= 2) {
        let t: Vec<char> = tok.chars().map(|c| c.to_ascii_lowercase()).collect();
        if t.len() > lower.len() {
            continue;
        }
        for i in 0..=lower.len() - t.len() {
            if lower[i..i + t.len()] == t[..] {
                mark[i..i + t.len()].iter_mut().for_each(|m| *m = true);
            }
        }
    }
    let mut out: Vec<(String, bool)> = Vec::new();
    for (c, m) in chars.into_iter().zip(mark) {
        match out.last_mut() {
            Some((s, lm)) if *lm == m => s.push(c),
            _ => out.push((c.to_string(), m)),
        }
    }
    out
}

const RED: Color = Color::Rgb(0xb0, 0x20, 0x28);

fn draw(f: &mut Frame, app: &App) {
    let [head, list, detail, input] = Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(3), Constraint::Length(1)]).areas(f.area());
    let w = f.area().width as usize;

    // header: tabs + filter chips + keys
    let tab = |m: Mode, name: &str| {
        let st = if app.mode == m { Style::default().fg(Color::White).bg(RED).add_modifier(Modifier::BOLD) } else { Style::default().fg(Color::DarkGray) };
        Span::styled(format!(" {name} "), st)
    };
    let chip = |k: &str, v: &str| vec![Span::styled(format!(" {k}:"), Style::default().fg(Color::DarkGray)), Span::styled(v.to_string(), Style::default().fg(Color::Yellow))];
    let mut spans = vec![Span::styled(" reman ", Style::default().fg(Color::White).bg(RED).add_modifier(Modifier::BOLD)), Span::raw(" "), tab(Mode::Recall, "Recall"), tab(Mode::Fixes, "Fixes"), tab(Mode::Flows, "Flows"), Span::raw(" ")];
    spans.extend(chip("scope", app.scope.name()));
    spans.extend(chip("actor", app.actor));
    spans.extend(chip("pass", app.status));
    if app.mode == Mode::Recall && !app.query.trim().is_empty() && app.group {
        spans.extend(chip("group", "on"));
    }
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let keys = "  ^T tab  ←→ scope  Tab actor  F2 pass  ^G group  Del forget  ^P pin  Esc";
    if used + keys.chars().count() < w {
        spans.push(Span::styled(keys, Style::default().fg(Color::DarkGray)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), head);

    // result list, bottom-up: best / newest sits right above the input line
    let items = app.visible_items();
    let h = list.height as usize;
    let start = if app.sel >= h { app.sel + 1 - h } else { 0 };
    let mut lines: Vec<Line> = Vec::with_capacity(h);
    let cmd_w = w.saturating_sub(4 + 11);
    for y in 0..h {
        let idx = start + (h - 1 - y);
        let Some(it) = items.get(idx) else {
            lines.push(Line::raw(""));
            continue;
        };
        let sel = idx == app.sel;
        let base = if sel { Style::default().fg(Color::White).bg(RED) } else { Style::default().fg(Color::Gray) };
        let hi = if sel { Style::default().fg(Color::LightYellow).bg(RED).add_modifier(Modifier::BOLD) } else { Style::default().fg(Color::Yellow) };
        let (glyph, gcol) = if it.predicted {
            ("»", Color::Cyan)
        } else {
            match it.status.as_str() {
                "ok" => ("+", Color::Green),
                "fail" => ("x", Color::Red),
                "mixed" => ("~", Color::Yellow),
                "flow" => ("⇢", Color::Cyan),
                _ => (" ", Color::DarkGray),
            }
        };
        let mut row = vec![
            Span::styled(if sel { "> " } else { "  " }, base),
            Span::styled(glyph, Style::default().fg(gcol).bg(if sel { RED } else { Color::Reset })),
            Span::styled(if it.pinned { "★" } else { " " }, Style::default().fg(Color::Yellow).bg(if sel { RED } else { Color::Reset })),
        ];
        for (seg, m) in highlight(&fit(&clean(&it.command), cmd_w), &app.query) {
            row.push(Span::styled(seg, if m { hi } else { base }));
        }
        let (tag, tcol) = if app.mode == Mode::Flows {
            (it.actor.clone(), Color::Cyan)
        } else if it.actor.starts_with("agent") {
            (it.actor.trim_start_matches("agent:").to_string(), Color::Yellow)
        } else {
            ("you".to_string(), Color::Green)
        };
        row.push(Span::styled(format!(" {}", fit(&tag, 10)), Style::default().fg(tcol)));
        lines.push(Line::from(row));
    }
    f.render_widget(Paragraph::new(lines), list);

    // provenance pane
    let dim = Style::default().fg(Color::DarkGray);
    let mut d: Vec<Line> = Vec::new();
    if let Some(msg) = &app.message {
        d.push(Line::styled(format!(" {msg}"), Style::default().fg(Color::LightRed)));
    }
    if let Some(it) = items.get(app.sel) {
        let m = &it.meta;
        if app.mode == Mode::Flows {
            d.push(Line::styled(format!(" workflow run {} times, {} steps", m["count"], m["length"]), Style::default().fg(Color::Cyan)));
            d.push(Line::styled(format!(" {}", clean(&it.command)), dim));
        } else {
            let desc = m["reason"].as_str().map(|r| format!("next: {r}")).or_else(|| m["description"].as_str().map(|s| format!("# {s}")));
            d.push(Line::styled(format!(" {}", desc.unwrap_or_default()), Style::default().fg(Color::Rgb(0xd7, 0xd7, 0xaf))));
            let rate = m["success_rate"].as_f64().map(|r| format!("{}% ok", (r * 100.0).round())).unwrap_or_else(|| "?".into());
            let who = if it.actor.starts_with("agent") { it.actor.clone() } else { "you".into() };
            let mut l2 = format!(" runs {} · {} · last {} · {} · {} folder(s)", m["run_count"], rate, m["last_run"].as_str().unwrap_or("?"), who, m["folders"]);
            if m["proven"].as_bool() == Some(true) {
                l2 = format!(" PROVEN FIX ({}) ·{}", m["fix_confidence"].as_str().unwrap_or(""), l2);
            } else if let Some(t) = m["typo"].as_f64() {
                l2 = format!(" typo {t:.2} · intent {:.2} ·{l2}", m["intent_sim"].as_f64().unwrap_or(0.0));
            }
            d.push(Line::styled(l2, dim));
            let folders = app.details.get(&it.command).and_then(|v| v["folder_list"].as_array().cloned()).unwrap_or_default();
            let fl: Vec<&str> = folders.iter().filter_map(Value::as_str).collect();
            d.push(Line::styled(format!(" @ {}", if fl.is_empty() { m["cwd"].as_str().unwrap_or("?").to_string() } else { fl.join("  ·  ") }), dim));
        }
    } else {
        d.push(Line::styled(if app.label.is_empty() { " …" } else { " (no matches)" }, dim));
    }
    d.truncate(3);
    f.render_widget(Paragraph::new(d), detail);

    // input bar
    let count = if app.total > items.len() { format!(" {}/{} ", items.len(), app.total) } else { format!(" {} ", items.len()) };
    let mode = if app.label.is_empty() { String::new() } else { format!("[{}]", app.label) };
    let left = format!(" reman> {}", app.query);
    let right = format!("{mode}{count}");
    let pad = w.saturating_sub(left.chars().count() + right.chars().count());
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(left.clone(), Style::default().fg(Color::White).bg(RED).add_modifier(Modifier::BOLD)),
            Span::styled(" ".repeat(pad), Style::default().bg(RED)),
            Span::styled(right, Style::default().fg(Color::Gray).bg(RED)),
        ])),
        input,
    );
    f.set_cursor_position((input.x + left.chars().count().min(w.saturating_sub(1)) as u16, input.y));
    let _ = Rect::default();
}

/// The terminal itself, never a std stream. Callers capture stdout (`$(reman find)`) and Windows
/// PowerShell 5.1 also redirects a native command's stderr inside PSReadLine key handlers, so
/// drawing on either can land in a pipe. CONOUT$ / /dev/tty always reach the screen.
fn tty() -> Result<std::fs::File> {
    let path = if cfg!(windows) { "CONOUT$" } else { "/dev/tty" };
    Ok(std::fs::OpenOptions::new().read(true).write(true).open(path)?)
}

/// Console output code page -> UTF-8 while the finder is up (the frame is UTF-8; a legacy OEM
/// code page turns `»` into `Γ├`), restored on drop.
struct Utf8Console(#[allow(dead_code)] u32);

impl Utf8Console {
    fn enable() -> Self {
        #[cfg(windows)]
        unsafe {
            let prev = win::GetConsoleOutputCP();
            win::SetConsoleOutputCP(65001);
            return Utf8Console(prev);
        }
        #[cfg(not(windows))]
        Utf8Console(0)
    }
}

impl Drop for Utf8Console {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            if self.0 != 0 {
                win::SetConsoleOutputCP(self.0);
            }
        }
    }
}

#[cfg(windows)]
mod win {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub fn GetConsoleOutputCP() -> u32;
        pub fn SetConsoleOutputCP(cp: u32) -> i32;
    }
}

pub fn run(o: Opts) -> Result<()> {
    let (jtx, jrx) = channel();
    let (rtx, rrx) = channel();
    std::thread::spawn(move || worker(jrx, rtx));
    let scope = match o.scope.as_str() {
        "folder" | "here" => ScopeSel::Folder,
        "repo" => ScopeSel::Repo,
        _ => ScopeSel::All,
    };
    let mut app = App {
        query: o.query,
        mode: Mode::Recall,
        scope,
        actor: "all",
        status: "all",
        group: true,
        cwd: o.cwd,
        items: vec![],
        total: 0,
        label: String::new(),
        sel: 0,
        want: 200,
        seq: 0,
        shown_seq: 0,
        details: HashMap::new(),
        confirm_forget: None,
        message: None,
        jobs: jtx,
    };
    app.refresh();

    let _cp = Utf8Console::enable();
    let mut out = tty()?;
    enable_raw_mode()?;
    execute!(out, EnterAlternateScreen, cursor::Show)?;
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        if let Ok(mut t) = tty() {
            let _ = execute!(t, LeaveAlternateScreen, cursor::Show);
        }
        // the alternate screen hides panics; keep a record
        let _ = std::fs::write(crate::config::home().join("tui-panic.log"), format!("{info}\n{}", std::backtrace::Backtrace::force_capture()));
        prev_hook(info);
    }));
    // one buffered write per frame; an unbuffered console handle costs a syscall per cell run
    let mut term = Terminal::new(CrosstermBackend::new(std::io::BufWriter::with_capacity(1 << 16, tty()?)))?;
    let chosen = event_loop(&mut term, &mut app, &rrx);
    disable_raw_mode()?;
    execute!(out, LeaveAlternateScreen, cursor::Show)?;
    let chosen = chosen?;
    if let Some(c) = chosen {
        match o.result_file {
            Some(p) => std::fs::write(p, c)?,
            None => println!("{c}"),
        }
    }
    Ok(())
}

fn event_loop(term: &mut Terminal<CrosstermBackend<std::io::BufWriter<std::fs::File>>>, app: &mut App, replies: &Receiver<Reply>) -> Result<Option<String>> {
    let mut dirty = true;
    loop {
        while let Ok(r) = replies.try_recv() {
            dirty = true;
            match r {
                Reply::Results(seq, items, total, label) if seq >= app.shown_seq => {
                    app.shown_seq = seq;
                    app.total = total.max(items.len());
                    app.items = items;
                    app.label = label;
                    app.sel = app.sel.min(app.visible_items().len().saturating_sub(1));
                }
                Reply::Results(..) => {}
                Reply::Detail(cmd, v) => {
                    app.details.insert(cmd, v);
                }
                Reply::Done("forget", v) => {
                    app.message = Some(format!("forgotten ({} row(s) deleted)", v["removed"]));
                    app.refresh();
                }
                Reply::Done(_, _) => app.refresh(),
                Reply::Error(e) => app.message = Some(e),
            }
        }
        if let Some(it) = app.selected() {
            if app.mode != Mode::Flows && !app.details.contains_key(&it.command) {
                app.details.insert(it.command.clone(), Value::Null);
                let _ = app.jobs.send(Job::Other("detail", json!({"op": "detail", "command": it.command})));
            }
        }
        if dirty {
            term.draw(|f| draw(f, app))?;
            dirty = false;
        }
        if !event::poll(Duration::from_millis(16))? {
            continue;
        }
        let ev = event::read()?;
        dirty = true;
        let Event::Key(k) = ev else { continue };
        if k.kind != KeyEventKind::Press {
            continue; // Windows reports press AND release
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let n = app.visible_items().len();
        if k.code != KeyCode::Delete {
            app.confirm_forget = None;
        }
        match k.code {
            KeyCode::Esc => return Ok(None),
            KeyCode::Char('c') | KeyCode::Char('d') if ctrl => return Ok(None),
            KeyCode::Enter => return Ok(app.selected().map(|i| i.command)),
            KeyCode::Up => app.sel = (app.sel + 1).min(n.saturating_sub(1)),
            KeyCode::Down => app.sel = app.sel.saturating_sub(1),
            KeyCode::PageUp => app.sel = (app.sel + 10).min(n.saturating_sub(1)),
            KeyCode::PageDown => app.sel = app.sel.saturating_sub(10),
            KeyCode::Left | KeyCode::Right => {
                let fwd = k.code == KeyCode::Right;
                app.scope = match (app.scope, fwd) {
                    (ScopeSel::Folder, true) | (ScopeSel::All, false) => ScopeSel::Repo,
                    (ScopeSel::Repo, true) | (ScopeSel::Folder, false) => ScopeSel::All,
                    _ => ScopeSel::Folder,
                };
                app.reset();
            }
            KeyCode::Tab => {
                app.actor = match app.actor {
                    "all" => "you",
                    "you" => "agent",
                    _ => "all",
                };
                app.reset();
            }
            KeyCode::F(2) => {
                app.status = match app.status {
                    "all" => "ok",
                    "ok" => "fail",
                    _ => "all",
                };
                app.reset();
            }
            KeyCode::Char('t') if ctrl => {
                app.mode = match app.mode {
                    Mode::Recall => Mode::Fixes,
                    Mode::Fixes => Mode::Flows,
                    Mode::Flows => Mode::Recall,
                };
                app.label.clear();
                app.reset();
            }
            KeyCode::Char('g') if ctrl => {
                app.group = !app.group;
                app.reset();
            }
            KeyCode::Char('p') if ctrl => {
                if let Some(it) = app.selected().filter(|_| app.mode != Mode::Flows) {
                    let _ = app.jobs.send(Job::Other("pin", json!({"op": "pin", "command": it.command, "on": !it.pinned})));
                }
            }
            KeyCode::Delete => {
                if let Some(it) = app.selected().filter(|_| app.mode != Mode::Flows) {
                    if app.confirm_forget.as_deref() == Some(it.command.as_str()) {
                        let _ = app.jobs.send(Job::Other("forget", json!({"op": "forget", "command": it.command})));
                        app.confirm_forget = None;
                    } else {
                        app.message = Some("press Del again to forget this command everywhere".into());
                        app.confirm_forget = Some(it.command);
                    }
                }
            }
            KeyCode::Char('u') if ctrl => {
                app.query.clear();
                app.reset();
            }
            KeyCode::Char('w') if ctrl => {
                let t = app.query.trim_end().to_string();
                app.query = t.rfind(' ').map(|i| t[..=i].to_string()).unwrap_or_default();
                app.reset();
            }
            KeyCode::Backspace => {
                if app.query.pop().is_some() {
                    app.reset();
                }
            }
            KeyCode::Char(c) if !ctrl => {
                app.query.push(c);
                app.message = None;
                if app.mode == Mode::Flows {
                    app.sel = 0; // flows filter locally
                } else {
                    app.reset();
                }
            }
            _ => {}
        }
        // near the end of what we have and more exists -> fetch a bigger page
        if app.mode == Mode::Recall && app.sel + 20 >= app.items.len() && app.total > app.items.len() && app.want <= app.items.len() {
            app.want += 400;
            app.refresh();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlights_tokens() {
        let h = highlight("docker compose up", "comp UP");
        assert_eq!(h, vec![("docker ".into(), false), ("comp".into(), true), ("ose ".into(), false), ("up".into(), true)]);
        assert_eq!(fit("abcdef", 4), "abc…");
        assert_eq!(fit("ab", 4), "ab  ");
    }
}
