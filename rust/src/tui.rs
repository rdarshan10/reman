//! `reman find` - the native finder, identical on pwsh / bash / zsh / fish.
//!
//! Design: a command palette, not a history dump. The query sits on top; results come in labelled
//! sections (the fix for the command that just failed, what you usually run next, this folder,
//! then other folders - so a folder with no match never dead-ends); and a card explains the
//! selected command: what it does, whether it worked, where it ran, and why it matched.
//!
//! Plumbing: draws on the terminal device (stdout stays free for the pick, so `$(reman find)`
//! works), talks to the daemon from a worker thread (typing never blocks; stale searches are
//! dropped), only fetches a page of results, and repaints whole lines (see `Screen`).
use crate::client::Client;
use anyhow::Result;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use ratatui::crossterm::{cursor, execute};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget, Wrap};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;
use unicode_width::UnicodeWidthStr;

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
    fn phrase(self) -> &'static str {
        match self {
            ScopeSel::Folder => "in this folder",
            ScopeSel::Repo => "in this repo",
            ScopeSel::All => "everywhere",
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    /// `op: next` - predictions, plus the fix when the session's last command failed
    Next,
    List,
    Flows,
}

/// One labelled block of results. `widen` = only run when fewer than N items came before
/// (the "nothing here yet, so here's everywhere" fallback).
struct Section {
    title: String,
    kind: Kind,
    req: Value,
    widen: Option<usize>,
    main: bool,
    /// keep only strong matches, at most N: a weak match from this folder must not sit above a
    /// strong one from elsewhere
    strong: Option<usize>,
    /// browsing with actor = anyone: an agent's one-off exploration (`cd x && grep ...`) buries
    /// what you ran; hide it and say so (tab -> agents shows it)
    hide_oneoffs: bool,
}

fn agent_oneoff(v: &Value) -> bool {
    v["actor"].as_str().is_some_and(|a| a.starts_with("agent")) && v["runs"].as_u64().unwrap_or(0) <= 1
}

/// A search hit that clearly matches: close in meaning, or the typed text really matches.
fn strong_hit(v: &Value, mode: &str) -> bool {
    if let Some(typo) = v["typo"].as_f64() {
        // did-you-mean: a proven fix, a near-typo, or clearly the same intent
        return v["proven"].as_bool() == Some(true) || typo >= 0.5 || v["intent_sim"].as_f64().unwrap_or(0.0) >= 0.75;
    }
    // an agent's one-off (`cd x; echo ===; grep ...`) is exploration, not something to rerun
    if agent_oneoff(v) {
        return false;
    }
    mode == "fuzzy" || mode == "manual" || v["similarity"].as_f64().unwrap_or(0.0) >= 0.64 || v["fuzzy"].as_f64().unwrap_or(0.0) >= 0.5
}

#[derive(Clone, Default)]
struct Item {
    command: String,
    actor: String,
    status: String,
    predicted: bool,
    fix: bool,
    pinned: bool,
    section: usize,
    meta: Value,
}

enum Job {
    Search(u64, Vec<Section>),
    Other(&'static str, Value),
}

struct Results {
    seq: u64,
    items: Vec<Item>,
    titles: Vec<String>,
    total: usize,
    label: String,
}

enum Reply {
    Results(Results),
    Detail(String, Value),
    /// (failed command, what worked instead or null)
    Fix(String, Value),
    Done(&'static str, Value),
    Error(String),
}

pub struct Opts {
    pub query: String,
    pub scope: String,
    pub cwd: String,
    pub result_file: Option<String>,
}

fn item_of(x: &Value) -> Item {
    Item {
        command: x["command"].as_str().unwrap_or("").to_string(),
        actor: x["actor"].as_str().unwrap_or("human").to_string(),
        status: x["status"].as_str().unwrap_or("unknown").to_string(),
        pinned: x["pinned"].as_bool().unwrap_or(false),
        predicted: x["predicted"].as_bool().unwrap_or(false),
        meta: x.clone(),
        ..Default::default()
    }
}

fn list_of(v: &Value) -> Vec<Item> {
    v["results"].as_array().map(|a| a.iter().map(item_of).collect()).unwrap_or_default()
}

fn flows_of(v: &Value) -> Vec<Item> {
    v["results"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|f| {
                    let steps: Vec<&str> = f["sequence"].as_array().map(|s| s.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                    Item { command: steps.join(" ; "), status: "flow".into(), meta: f.clone(), ..Default::default() }
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
                Job::Search(seq, sections) => {
                    if Some(i) != last_search {
                        continue;
                    }
                    let res = (|| -> Result<Results> {
                        let mut r = Results { seq, items: Vec::new(), titles: Vec::new(), total: 0, label: String::new() };
                        let push = |r: &mut Results, title: String, items: Vec<Item>| {
                            let fresh: Vec<Item> = items.into_iter().filter(|it| !r.items.iter().any(|x| x.command == it.command)).collect();
                            if fresh.is_empty() {
                                return;
                            }
                            let s = r.titles.len();
                            r.titles.push(title);
                            r.items.extend(fresh.into_iter().map(|mut it| {
                                it.section = s;
                                it
                            }));
                        };
                        for sec in sections {
                            if sec.widen.is_some_and(|n| r.items.len() >= n) {
                                continue;
                            }
                            let v = call(&sec.req)?;
                            match sec.kind {
                                Kind::Next => {
                                    if let Some(fx) = v.get("fix").filter(|f| f.is_object()) {
                                        let mut it = item_of(&fx["item"]);
                                        it.fix = true;
                                        let failed = fx["failed"].as_str().unwrap_or("");
                                        push(&mut r, format!("last command failed: {}", one_line(failed)), vec![it]);
                                    }
                                    let mut next = list_of(&v);
                                    next.retain(|it| !agent_oneoff(&it.meta));
                                    push(&mut r, sec.title, next);
                                }
                                Kind::List | Kind::Flows => {
                                    let mut items = if sec.kind == Kind::Flows { flows_of(&v) } else { list_of(&v) };
                                    if let Some(cap) = sec.strong {
                                        let mode = v["mode"].as_str().unwrap_or("");
                                        // never lead with a command that only ever failed (unless failures are what you asked for)
                                        let want_fail = sec.req["status"] == "fail";
                                        items.retain(|it| strong_hit(&it.meta, mode) && (want_fail || it.status != "fail"));
                                        items.truncate(cap);
                                    }
                                    let mut title = sec.title;
                                    if sec.hide_oneoffs {
                                        let before = items.len();
                                        items.retain(|it| !agent_oneoff(&it.meta));
                                        let hidden = before - items.len();
                                        if hidden > 0 {
                                            title = format!("{title} · {hidden} agent one-off{} hidden (tab)", if hidden == 1 { "" } else { "s" });
                                        }
                                    }
                                    if sec.main {
                                        r.total = v["total"].as_u64().unwrap_or(items.len() as u64) as usize;
                                        r.label = v["mode"].as_str().unwrap_or("").to_string();
                                    }
                                    push(&mut r, title, items);
                                }
                            }
                        }
                        Ok(r)
                    })();
                    let _ = tx.send(match res {
                        Ok(r) => Reply::Results(r),
                        Err(e) => Reply::Error(e.to_string()),
                    });
                }
                Job::Other(tag, req) => {
                    let reply = match call(&req) {
                        Ok(v) if tag == "detail" => Reply::Detail(req["command"].as_str().unwrap_or("").to_string(), v),
                        Ok(v) if tag == "fixfor" => Reply::Fix(req["command"].as_str().unwrap_or("").to_string(), v["suggest"].clone()),
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
    session: String,
    items: Vec<Item>,
    titles: Vec<String>,
    total: usize,
    label: String,
    loaded: bool,
    sel: usize,
    want: usize,
    seq: u64,
    shown_seq: u64,
    details: HashMap<String, Value>,
    /// failed command -> what worked instead (null = nothing known), for the Fixes tab
    fixes: HashMap<String, Value>,
    confirm_forget: Option<String>,
    message: Option<String>,
    help: bool,
    jobs: Sender<Job>,
}

impl App {
    fn refresh(&mut self) {
        self.seq += 1;
        let q = self.query.trim().to_string();
        let scoped = |mut r: Value, scope: ScopeSel, cwd: &str| {
            r["cwd"] = json!(cwd);
            r["scope"] = json!(scope.name());
            r
        };
        let filters = |mut r: Value, actor: &str, status: &str| {
            if actor != "all" {
                r["actor"] = json!(if actor == "you" { "human" } else { "agent" });
            }
            if status != "all" {
                r["status"] = json!(status);
            }
            r
        };
        let sec = |title: &str, kind: Kind, req: Value, widen: Option<usize>, main: bool| Section { title: title.to_string(), kind, req, widen, main, strong: None, hide_oneoffs: false };
        let narrow = self.scope != ScopeSel::All;
        let mut s: Vec<Section> = Vec::new();
        match self.mode {
            Mode::Recall if q.is_empty() => {
                if self.actor == "all" && self.status == "all" {
                    s.push(sec("likely next", Kind::Next, json!({"op": "next", "cwd": self.cwd, "session": self.session, "k": 3}), None, false));
                }
                let recent = filters(json!({"op": "recent", "k": self.want}), self.actor, self.status);
                let mut here = sec(&format!("recent {}", self.scope.phrase()), Kind::List, scoped(recent.clone(), self.scope, &self.cwd), None, true);
                here.hide_oneoffs = self.actor == "all";
                s.push(here);
                if narrow {
                    let mut all = sec("recent everywhere - nothing ran here yet", Kind::List, scoped(recent, ScopeSel::All, &self.cwd), Some(4), false);
                    all.hide_oneoffs = self.actor == "all";
                    s.push(all);
                }
            }
            Mode::Recall => {
                // this folder's STRONG matches first, then everything ranked across all folders
                let find = filters(json!({"op": "search", "query": q, "k": self.want, "group": self.group}), self.actor, self.status);
                if narrow {
                    let mut here = sec(self.scope.phrase(), Kind::List, scoped(find.clone(), self.scope, &self.cwd), None, false);
                    here.req["k"] = json!(40);
                    here.strong = Some(8);
                    s.push(here);
                }
                s.push(sec("everywhere", Kind::List, scoped(find, ScopeSel::All, &self.cwd), None, true));
            }
            Mode::Fixes if q.is_empty() => {
                let fails = json!({"op": "recent", "k": self.want, "status": "fail"});
                s.push(sec(&format!("commands that failed {}", self.scope.phrase()), Kind::List, scoped(fails.clone(), self.scope, &self.cwd), None, true));
                if narrow {
                    s.push(sec("failed elsewhere", Kind::List, scoped(fails, ScopeSel::All, &self.cwd), Some(1), false));
                }
            }
            Mode::Fixes => {
                let mut r = json!({"op": "didyoumean", "query": q, "k": 30, "worked_only": true});
                if self.scope == ScopeSel::Folder {
                    r["here"] = json!(self.cwd);
                }
                let mut fx = sec(&format!("what worked instead of: {q}"), Kind::List, r, None, true);
                fx.strong = Some(30);
                s.push(fx);
            }
            Mode::Flows => {
                let mut r = json!({"op": "flows", "k": 60});
                if self.scope == ScopeSel::Folder {
                    r["cwd"] = json!(self.cwd);
                }
                s.push(sec(&format!("sequences you repeat {}", if self.scope == ScopeSel::Folder { "in this folder" } else { "everywhere" }), Kind::Flows, r, None, true));
                if self.scope == ScopeSel::Folder {
                    s.push(sec("sequences you repeat everywhere", Kind::Flows, json!({"op": "flows", "k": 60}), Some(1), false));
                }
            }
        }
        let _ = self.jobs.send(Job::Search(self.seq, s));
    }

    fn visible_items(&self) -> Vec<&Item> {
        if self.mode != Mode::Flows || self.query.trim().is_empty() {
            return self.items.iter().collect();
        }
        let toks: Vec<String> = self.query.split_whitespace().map(|t| t.to_lowercase()).collect();
        self.items
            .iter()
            .filter(|it| {
                let l = it.command.to_lowercase();
                toks.iter().all(|t| l.contains(t.as_str()))
            })
            .collect()
    }

    fn selected(&self) -> Option<Item> {
        self.visible_items().get(self.sel).map(|x| (*x).clone())
    }

    /// Fixes tab, browsing failures: the selected failure's known fix, if any.
    fn fix_of(&self, it: &Item) -> Option<String> {
        if self.mode != Mode::Fixes || !self.query.trim().is_empty() {
            return None;
        }
        self.fixes.get(&it.command).and_then(|f| f["command"].as_str()).map(String::from)
    }

    /// What Enter puts on the prompt: the fix when a failure with a known fix is selected.
    fn pick(&self) -> Option<String> {
        let it = self.selected()?;
        Some(self.fix_of(&it).unwrap_or(it.command))
    }

    fn reset(&mut self) {
        self.sel = 0;
        self.want = 200;
        self.confirm_forget = None;
        self.refresh();
    }
}

// ---------------------------------------------------------------------------------------------
// text helpers
// ---------------------------------------------------------------------------------------------

/// Display-safe text: control characters become spaces, newlines a visible `↵`, and zero-width
/// characters (combining marks, joiners, variation selectors) are dropped - terminals disagree
/// on how wide those clusters are, and a disagreement shifts the rest of the line.
fn clean(s: &str) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut out = String::with_capacity(s.len());
    for c in s.trim_end().chars() {
        match c {
            '\n' => out.push_str(" ↵ "),
            '\r' => {}
            c if c.is_control() => out.push(' '),
            c if !c.is_ascii() && c.width().unwrap_or(0) == 0 => {}
            c => out.push(c),
        }
    }
    out
}

fn one_line(s: &str) -> String {
    let c = clean(s);
    if c.chars().count() > 60 { format!("{}…", c.chars().take(59).collect::<String>()) } else { c }
}

/// Pad or cut to exactly `w` display columns.
fn fit(s: &str, w: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    let total = UnicodeWidthStr::width(s);
    let budget = if total > w { w.saturating_sub(1) } else { w };
    for c in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + cw > budget {
            break;
        }
        out.push(c);
        used += cw;
    }
    if total > w && w > 0 {
        out.push('…');
        used += 1;
    }
    out.push_str(&" ".repeat(w.saturating_sub(used)));
    out
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

/// "now", "5m", "3h", "2d", "3w", "4mo", "2y"
fn ago_short(ts: i64) -> String {
    if ts <= 0 {
        return String::new();
    }
    let d = (crate::config::now() - ts).max(0);
    match d {
        0..60 => "now".into(),
        60..3600 => format!("{}m", d / 60),
        3600..86400 => format!("{}h", d / 3600),
        86400..604800 => format!("{}d", d / 86400),
        604800..2592000 => format!("{}w", d / 604800),
        2592000..31536000 => format!("{}mo", d / 2592000),
        _ => format!("{}y", d / 31536000),
    }
}

fn ago_long(ts: i64) -> String {
    if ts <= 0 {
        return "at an unknown time".into();
    }
    let d = (crate::config::now() - ts).max(0);
    let (n, unit) = match d {
        0..60 => return "just now".into(),
        60..3600 => (d / 60, "minute"),
        3600..86400 => (d / 3600, "hour"),
        86400..604800 => (d / 86400, "day"),
        604800..2592000 => (d / 604800, "week"),
        2592000..31536000 => (d / 2592000, "month"),
        _ => (d / 31536000, "year"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

/// "you", or the agent's short name (`agent:claude-code` -> `claude`)
fn who(actor: &str) -> String {
    if !actor.starts_with("agent") {
        return "you".into();
    }
    let a = actor.trim_start_matches("agent:").trim_start_matches("agent");
    let a = a.strip_suffix("-code").or_else(|| a.strip_suffix("-cli")).or_else(|| a.strip_suffix("-desktop")).unwrap_or(a);
    if a.is_empty() { "agent".into() } else { a.to_string() }
}

// ---------------------------------------------------------------------------------------------
// drawing
// ---------------------------------------------------------------------------------------------

// Named ANSI colours only: the terminal's theme maps them, so the finder reads the same on a
// dark or a light background (RGB pastels vanish on white).
const ACCENT: Color = Color::Blue;
const OK: Color = Color::Green;
const BAD: Color = Color::Red;
const WARN: Color = Color::Yellow;
const INFO: Color = Color::Cyan;
const MUTED: Color = Color::DarkGray;

fn muted() -> Style {
    Style::default().fg(MUTED)
}

fn glyph(it: &Item) -> (&'static str, Color) {
    if it.fix {
        return ("→", OK);
    }
    if it.predicted {
        return ("»", INFO);
    }
    match it.status.as_str() {
        "ok" => ("✓", OK),
        "fail" => ("✗", BAD),
        "mixed" => ("~", WARN),
        "flow" => ("⇢", INFO),
        _ => ("·", MUTED),
    }
}

fn draw(buf: &mut Buffer, app: &App) -> (u16, u16) {
    let area = buf.area;
    let (w, h) = (area.width as usize, area.height);
    if w < 20 || h < 6 {
        Paragraph::new("reman: terminal too small").render(area, buf);
        return (0, 0);
    }
    let items = app.visible_items();

    // row 0: the query, with the tabs on the right
    let tabs: Vec<(Mode, &str)> = vec![(Mode::Recall, "Recall"), (Mode::Fixes, "Fixes"), (Mode::Flows, "Flows")];
    let tabs_w: usize = tabs.iter().map(|t| t.1.len() + 2).sum::<usize>() + 4;
    let prompt = " › ";
    let mut row0 = vec![Span::styled(prompt, Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))];
    let qw = w.saturating_sub(prompt.len() + tabs_w + 1);
    let shown_q: String = {
        let n = app.query.chars().count();
        if n > qw { app.query.chars().skip(n - qw).collect() } else { app.query.clone() }
    };
    if app.query.is_empty() {
        let hint = match app.mode {
            Mode::Recall => "describe it in words, or type part of the command",
            Mode::Fixes => "paste or type the command that failed",
            Mode::Flows => "filter sequences",
        };
        row0.push(Span::styled(fit(hint, qw), muted()));
    } else {
        row0.push(Span::styled(fit(&shown_q, qw), Style::default().add_modifier(Modifier::BOLD)));
    }
    row0.push(Span::raw(" "));
    for (m, name) in &tabs {
        let st = if app.mode == *m { Style::default().fg(ACCENT).add_modifier(Modifier::BOLD | Modifier::UNDERLINED) } else { muted() };
        row0.push(Span::styled(format!(" {name} "), st));
    }
    Paragraph::new(Line::from(row0)).render(Rect::new(0, 0, area.width, 1), buf);
    let cursor = ((prompt.chars().count() + UnicodeWidthStr::width(shown_q.as_str())).min(w - 1) as u16, 0);

    // row 1: the filters as a sentence, each with the key that changes it
    let val = |on: bool, s: &str| Span::styled(s.to_string(), if on { Style::default().fg(ACCENT) } else { Style::default() });
    let key = |k: &str| Span::styled(format!(" {k}"), muted());
    let dot = || Span::styled("  ·  ", muted());
    // with a query, folder/repo scope means "these first", never "only these"
    let scope_txt = match (app.mode == Mode::Recall && !app.query.trim().is_empty(), app.scope) {
        (true, ScopeSel::Folder) => "this folder first",
        (true, ScopeSel::Repo) => "this repo first",
        (_, s) => s.phrase(),
    };
    let mut row1 = vec![Span::raw("   "), val(app.scope != ScopeSel::Folder, scope_txt), key("←→")];
    if app.mode == Mode::Recall {
        let actor = match app.actor {
            "you" => "by you",
            "agent" => "by agents",
            _ => "by anyone",
        };
        let status = match app.status {
            "ok" => "worked",
            "fail" => "failed",
            _ => "any outcome",
        };
        row1.extend([dot(), val(app.actor != "all", actor), key("tab"), dot(), val(app.status != "all", status), key("F2")]);
        if !app.query.trim().is_empty() {
            row1.extend([dot(), val(!app.group, if app.group { "variants folded" } else { "every variant" }), key("^G")]);
        }
    }
    let count = if !app.loaded {
        "…".to_string()
    } else if app.total > items.len() {
        format!("{} of {} ", items.len(), app.total)
    } else if items.len() == 1 {
        "1 result ".to_string()
    } else {
        format!("{} results ", items.len())
    };
    let used: usize = row1.iter().map(|s| UnicodeWidthStr::width(s.content.as_ref())).sum();
    if used + count.len() + 1 < w {
        row1.push(Span::raw(" ".repeat(w - used - count.len())));
        row1.push(Span::styled(count, muted()));
    }
    Paragraph::new(Line::from(row1)).render(Rect::new(0, 1, area.width, 1), buf);

    // body: list + detail card (right column when wide, below when narrow)
    let footer_y = h - 1;
    let body_top = 2u16;
    let wide = w >= 118 && h >= 12;
    let (list, card) = if wide {
        let cw = (w as u16 * 2 / 5).clamp(40, 64);
        (Rect::new(0, body_top, area.width - cw, footer_y - body_top), Rect::new(area.width - cw, body_top, cw, footer_y - body_top))
    } else {
        let ch = if h >= 18 { 6 } else { 4 };
        let lh = footer_y.saturating_sub(body_top + ch);
        (Rect::new(0, body_top, area.width, lh), Rect::new(0, body_top + lh, area.width, ch))
    };
    draw_list(buf, app, &items, list);
    draw_card(buf, app, &items, card, wide);

    // footer: the keys that matter now, or a message
    let footer = if let Some(c) = &app.confirm_forget {
        Line::from(vec![Span::styled(format!(" Del again forgets `{}` everywhere", one_line(c)), Style::default().fg(BAD).add_modifier(Modifier::BOLD)), Span::styled("  ·  any other key keeps it", muted())])
    } else if let Some(m) = &app.message {
        Line::from(Span::styled(format!(" {m}"), Style::default().fg(WARN)))
    } else {
        let enter = if app.selected().is_some_and(|it| app.fix_of(&it).is_some()) { "insert the fix" } else { "insert" };
        let mut k: Vec<(&str, &str)> = vec![("↵", enter), ("↑↓", "move"), ("^T", next_tab(app.mode))];
        if app.mode != Mode::Flows {
            k.extend([("Del", "forget"), ("^P", "pin")]);
        }
        k.extend([("F1", "all keys"), ("esc", "close")]);
        let mut sp = vec![Span::raw(" ")];
        for (key, what) in k {
            sp.push(Span::styled(key, Style::default().add_modifier(Modifier::BOLD)));
            sp.push(Span::styled(format!(" {what}   "), muted()));
        }
        Line::from(sp)
    };
    Paragraph::new(footer).render(Rect::new(0, footer_y, area.width, 1), buf);

    if app.help {
        draw_help(buf, area);
    }
    cursor
}

fn next_tab(m: Mode) -> &'static str {
    match m {
        Mode::Recall => "fixes",
        Mode::Fixes => "flows",
        Mode::Flows => "recall",
    }
}

/// Rows of the list: section headers interleaved with items. Returns (lines, row of selection).
fn list_rows<'a>(app: &App, items: &[&'a Item], w: usize) -> (Vec<Line<'a>>, usize) {
    // one plain result list needs no heading; Fixes/Flows headings say what the list IS
    let headers = app.titles.len() > 1 || app.mode != Mode::Recall || items.first().is_some_and(|i| i.fix || i.predicted);
    let mut lines = Vec::new();
    let mut sel_row = 0;
    let mut cur: Option<usize> = None;
    // right-hand meta columns shrink away on narrow terminals
    let meta_w = if w >= 70 { 22 } else if w >= 50 { 12 } else { 0 };
    let cmd_w = w.saturating_sub(4 + meta_w + 1);
    for (idx, it) in items.iter().enumerate() {
        if headers && cur != Some(it.section) {
            cur = Some(it.section);
            let t = app.titles.get(it.section).cloned().unwrap_or_default();
            let t = format!("   {t} ");
            let rule = "─".repeat(w.saturating_sub(UnicodeWidthStr::width(t.as_str()) + 1));
            if !lines.is_empty() {
                lines.push(Line::raw(""));
            }
            lines.push(Line::from(vec![Span::styled(t, Style::default().fg(MUTED).add_modifier(Modifier::BOLD)), Span::styled(rule, muted())]));
        }
        let sel = idx == app.sel;
        if sel {
            sel_row = lines.len();
        }
        let (g, gc) = glyph(it);
        let text_st = if sel { Style::default().add_modifier(Modifier::BOLD) } else { Style::default() };
        let hi = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);
        let mut row = vec![
            Span::styled(if sel { " ▌" } else { "  " }, Style::default().fg(ACCENT)),
            Span::styled(g, Style::default().fg(gc)),
            Span::styled(if it.pinned { "*" } else { " " }, Style::default().fg(WARN)),
        ];
        let shown = if app.mode == Mode::Flows { it.command.replace(" ; ", "  →  ") } else { it.command.clone() };
        for (seg, m) in highlight(&fit(&clean(&shown), cmd_w), &app.query) {
            row.push(Span::styled(seg, if m { hi } else { text_st }));
        }
        if meta_w > 0 {
            row.push(Span::raw(" "));
            let m = &it.meta;
            if it.fix || it.predicted || app.mode == Mode::Flows {
                let note = if it.fix {
                    if m["proven"].as_bool() == Some(true) { "worked last time".to_string() } else { "closest you ran".to_string() }
                } else if app.mode == Mode::Flows {
                    format!("{}x · {} steps", m["count"], m["length"])
                } else {
                    m["reason"].as_str().unwrap_or("").to_string()
                };
                row.push(Span::styled(fit(&note, meta_w), Style::default().fg(if it.fix { OK } else { INFO })));
            } else {
                let runs = m["runs"].as_u64().unwrap_or(0);
                let age = ago_short(m["last_used"].as_i64().unwrap_or(0));
                if meta_w >= 22 {
                    let wname = who(&it.actor);
                    row.push(Span::styled(format!("{:>5} {:>4}  ", format!("{runs}×"), age), muted()));
                    let wc = if it.actor.starts_with("agent") { WARN } else { MUTED };
                    row.push(Span::styled(fit(&wname, 7), Style::default().fg(wc)));
                } else {
                    row.push(Span::styled(fit(&format!("{:>5} {:>4}", format!("{runs}×"), age), meta_w), muted()));
                }
            }
        }
        lines.push(Line::from(row));
    }
    (lines, sel_row)
}

fn draw_list(buf: &mut Buffer, app: &App, items: &[&Item], r: Rect) {
    let h = r.height as usize;
    if h == 0 {
        return;
    }
    if items.is_empty() {
        let (a, b) = empty_state(app);
        let lines = vec![Line::raw(""), Line::styled(format!("   {a}"), Style::default()), Line::styled(format!("   {b}"), muted())];
        Paragraph::new(lines).render(r, buf);
        return;
    }
    let (lines, sel_row) = list_rows(app, items, r.width as usize);
    // keep the selection (and its section header when it's the first item) in view
    let start = if sel_row + 2 > h { sel_row + 2 - h } else { 0 };
    let start = start.min(lines.len().saturating_sub(h).max(0));
    Paragraph::new(lines.into_iter().skip(start).take(h).collect::<Vec<_>>()).render(r, buf);
}

fn empty_state(app: &App) -> (String, String) {
    if !app.loaded {
        return ("searching…".into(), String::new());
    }
    let q = app.query.trim();
    match app.mode {
        Mode::Recall if q.is_empty() => ("No history yet.".into(), "Commands you run are recorded as you go.".into()),
        Mode::Recall => (
            format!("You haven't run anything like \"{q}\" yet."),
            if app.actor != "all" || app.status != "all" { "Filters are on: tab / F2 to widen them.".into() } else { "Try other words, or ^T for fixes and flows.".into() },
        ),
        Mode::Fixes if q.is_empty() => ("No failed commands recorded here.".into(), "←→ to look in other folders.".into()),
        Mode::Fixes => (format!("Nothing you ran looks like a fix for \"{q}\"."), "Fixes are learned when a failed command is followed by one that works.".into()),
        Mode::Flows => ("No repeated sequences yet.".into(), "A flow appears once you run the same steps a few times.".into()),
    }
}

fn draw_card(buf: &mut Buffer, app: &App, items: &[&Item], r: Rect, wide: bool) {
    if r.height == 0 {
        return;
    }
    // separator: a left rule when beside the list, a top rule when below it
    let inner = if wide {
        for y in r.y..r.y + r.height {
            buf.set_string(r.x, y, "│", muted());
        }
        Rect::new(r.x + 2, r.y, r.width.saturating_sub(3), r.height)
    } else {
        buf.set_string(r.x, r.y, "─".repeat(r.width as usize), muted());
        Rect::new(r.x + 1, r.y + 1, r.width.saturating_sub(2), r.height - 1)
    };
    let Some(it) = items.get(app.sel) else { return };
    let m = &it.meta;
    let mut l: Vec<Line> = Vec::new();
    let body = Style::default();
    if app.mode == Mode::Flows {
        l.push(Line::styled(format!("You ran these {} steps together {} times", m["length"], m["count"]), Style::default().fg(INFO)));
        let steps: Vec<String> = m["sequence"].as_array().map(|a| a.iter().filter_map(Value::as_str).map(one_line).collect()).unwrap_or_default();
        for (i, s) in steps.iter().enumerate() {
            l.push(Line::from(vec![Span::styled(format!("{}. ", i + 1), muted()), Span::styled(s.clone(), body)]));
        }
    } else {
        if wide {
            l.push(Line::styled(clean(&it.command), Style::default().add_modifier(Modifier::BOLD)));
        }
        // browsing failures: lead with what worked instead
        if app.mode == Mode::Fixes && app.query.trim().is_empty() {
            match (app.fix_of(it), app.fixes.get(&it.command)) {
                (Some(f), _) => {
                    l.push(Line::from(vec![Span::styled("What worked instead: ", Style::default().fg(OK)), Span::styled(clean(&f), Style::default().fg(OK).add_modifier(Modifier::BOLD))]));
                }
                (None, Some(v)) if v.is_null() => {
                    l.push(Line::styled("No fix known yet - nothing that worked followed it.", muted()));
                }
                _ => {}
            }
        }
        // what it is / why it's offered
        let why = if it.fix {
            let t = m["times_fixed"].as_u64().unwrap_or(0);
            if m["proven"].as_bool() == Some(true) { format!("Last time this failed, you ran this next and it worked{}.", if t > 1 { format!(" ({t}x)") } else { String::new() }) } else { "The closest command you've run that worked.".into() }
        } else if it.predicted {
            format!("Offered because {}.", m["reason"].as_str().unwrap_or("you often run it here"))
        } else if m["proven"].as_bool() == Some(true) {
            format!("Proven fix ({}): it followed the failure and worked.", m["fix_confidence"].as_str().unwrap_or("seen"))
        } else {
            m["description"].as_str().map(|d| if d.chars().count() > 180 { format!("{}…", d.chars().take(179).collect::<String>()) } else { d.to_string() }).unwrap_or_default()
        };
        if !why.is_empty() {
            l.push(Line::styled(why, Style::default().fg(if it.fix { OK } else { MUTED }).add_modifier(Modifier::ITALIC)));
        }
        // did it work
        let runs = m["runs"].as_u64().unwrap_or(0);
        let rate = m["success_rate"].as_f64();
        let (mark, col, verdict) = match (it.status.as_str(), rate) {
            ("ok", _) => ("✓", OK, format!("worked every time ({runs} run{})", if runs == 1 { "" } else { "s" })),
            ("fail", _) => ("✗", BAD, format!("failed every time ({runs} run{})", if runs == 1 { "" } else { "s" })),
            ("mixed", Some(r)) => ("~", WARN, format!("worked {}% of {runs} runs", (r * 100.0).round())),
            _ => ("·", MUTED, format!("ran {runs}x - outcome not recorded (old history)")),
        };
        let last = ago_long(m["last_used"].as_i64().unwrap_or(0));
        l.push(Line::from(vec![
            Span::styled(format!("{mark} "), Style::default().fg(col)),
            Span::styled(verdict, Style::default().fg(col)),
            Span::styled(format!(" · last {last} · by {}", who(&it.actor)), muted()),
        ]));
        // where
        let folders: Vec<String> = app.details.get(&it.command).and_then(|v| v["folder_list"].as_array().cloned()).unwrap_or_default().iter().filter_map(Value::as_str).filter(|f| !f.eq_ignore_ascii_case("unknown")).map(String::from).collect();
        let place = match (folders.as_slice(), m["cwd"].as_str()) {
            ([], Some(c)) if !c.eq_ignore_ascii_case("unknown") => format!("in {c}"),
            ([], _) => "folder not recorded (imported before reman tracked folders)".into(),
            ([one], _) => format!("in {one}"),
            (many, _) => format!("in {}  (+{} more)", many[0], many.len() - 1),
        };
        l.push(Line::styled(place, muted()));
        // why it matched
        if !app.query.trim().is_empty() && app.mode == Mode::Recall {
            let sim = m["similarity"].as_f64().unwrap_or(-1.0);
            let fz = m["fuzzy"].as_f64().unwrap_or(0.0);
            let mut why = Vec::new();
            // bge similarities run hot (unrelated text ~0.6), so speak in bands, not percentages
            match sim {
                s if s >= 0.8 => why.push("very close in meaning"),
                s if s >= 0.7 => why.push("close in meaning"),
                s if s >= 0.64 => why.push("related in meaning"),
                _ => {}
            }
            if fz >= 0.5 {
                why.push("the text matches what you typed");
            }
            let mut why: Vec<String> = why.into_iter().map(String::from).collect();
            if let Some(v) = m["variants"].as_u64() {
                why.push(format!("{v} variants folded (^G)"));
            }
            if !why.is_empty() {
                l.push(Line::styled(format!("matched: {}", why.join(" · ")), muted()));
            }
        }
    }
    let mut p = Paragraph::new(l);
    if wide {
        p = p.wrap(Wrap { trim: false });
    }
    p.render(inner, buf);
}

fn draw_help(buf: &mut Buffer, area: Rect) {
    let keys: &[(&str, &str)] = &[
        ("↵", "put the command on your prompt (it does not run)"),
        ("↑ ↓  PgUp PgDn", "move"),
        ("^T", "switch Recall → Fixes → Flows"),
        ("← →", "where: this folder → this repo → everywhere"),
        ("tab", "who: anyone → you → agents"),
        ("F2", "outcome: any → worked → failed"),
        ("^G", "fold variants of a command / show each"),
        ("^P", "pin: pinned commands rank first"),
        ("Del Del", "forget a command everywhere (e.g. it held a secret)"),
        ("^U  ^W", "clear the query / delete a word"),
        ("esc", "close"),
    ];
    let bw = (area.width as usize).min(74) as u16;
    let bh = (keys.len() as u16 + 4).min(area.height);
    let r = Rect::new((area.width - bw) / 2, (area.height - bh) / 2, bw, bh);
    ratatui::widgets::Clear.render(r, buf);
    let block = ratatui::widgets::Block::bordered()
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(" keys ", Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)))
        .title_bottom(Span::styled(" any key closes ", muted()));
    let inner = block.inner(r);
    block.render(r, buf);
    let mut l = vec![Line::raw("")];
    for (k, what) in keys {
        l.push(Line::from(vec![Span::styled(format!(" {k:<16}"), Style::default().add_modifier(Modifier::BOLD)), Span::styled(what.to_string(), muted())]));
    }
    Paragraph::new(l).render(inner, buf);
}

// ---------------------------------------------------------------------------------------------
// the screen
// ---------------------------------------------------------------------------------------------

/// Paints a ratatui Buffer one WHOLE line at a time: jump to the line, write every cell, reset,
/// erase to end of line. A line that differs from the last frame is always rewritten in full, so
/// when the terminal and we disagree on a character's width the damage is limited to that line
/// and cleared on its next change - no stale fragments (the cell-diff renderer could leave some).
/// One buffered write per frame; autowrap is off so an over-wide line can never scroll the view.
struct Screen {
    out: std::io::BufWriter<std::fs::File>,
    prev: Vec<String>,
}

impl Screen {
    fn invalidate(&mut self) {
        self.prev.clear();
        let _ = self.out.write_all(b"\x1b[0m\x1b[2J");
    }

    fn frame(&mut self, buf: &Buffer, cursor: (u16, u16)) -> std::io::Result<()> {
        let a = buf.area;
        self.prev.resize(a.height as usize, String::from("\u{0}"));
        self.out.write_all(b"\x1b[?25l")?;
        for y in 0..a.height {
            let line = row_ansi(buf, y);
            if self.prev[y as usize] != line {
                write!(self.out, "\x1b[{};1H{line}", y + 1)?;
                self.prev[y as usize] = line;
            }
        }
        write!(self.out, "\x1b[{};{}H\x1b[?25h", cursor.1 + 1, cursor.0 + 1)?;
        self.out.flush()
    }
}

fn sgr(st: (Color, Color, Modifier)) -> String {
    let mut p = vec!["0".to_string()];
    let m = st.2;
    for (bit, code) in [(Modifier::BOLD, "1"), (Modifier::DIM, "2"), (Modifier::ITALIC, "3"), (Modifier::UNDERLINED, "4"), (Modifier::REVERSED, "7")] {
        if m.contains(bit) {
            p.push(code.into());
        }
    }
    let col = |c: Color, fg: bool| -> Option<String> {
        let base = if fg { 30 } else { 40 };
        Some(match c {
            Color::Reset => return None,
            Color::Black => format!("{}", base),
            Color::Red => format!("{}", base + 1),
            Color::Green => format!("{}", base + 2),
            Color::Yellow => format!("{}", base + 3),
            Color::Blue => format!("{}", base + 4),
            Color::Magenta => format!("{}", base + 5),
            Color::Cyan => format!("{}", base + 6),
            Color::Gray => format!("{}", base + 7),
            Color::DarkGray => format!("{}", base + 60),
            Color::LightRed => format!("{}", base + 61),
            Color::LightGreen => format!("{}", base + 62),
            Color::LightYellow => format!("{}", base + 63),
            Color::LightBlue => format!("{}", base + 64),
            Color::LightMagenta => format!("{}", base + 65),
            Color::LightCyan => format!("{}", base + 66),
            Color::White => format!("{}", base + 67),
            Color::Rgb(r, g, b) => format!("{};2;{r};{g};{b}", base + 8),
            Color::Indexed(i) => format!("{};5;{i}", base + 8),
        })
    };
    p.extend(col(st.0, true));
    p.extend(col(st.1, false));
    format!("\x1b[{}m", p.join(";"))
}

/// One line as text + SGR. Trailing default blanks are left to EL; a line that fills the last
/// column skips EL (at the pending-wrap position EL would erase that final cell).
fn row_ansi(buf: &Buffer, y: u16) -> String {
    let a = buf.area;
    let plain = |c: &ratatui::buffer::Cell| c.symbol() == " " && c.bg == Color::Reset && !c.modifier.contains(Modifier::UNDERLINED | Modifier::REVERSED);
    let mut end = a.width;
    while end > 0 && buf.cell((end - 1, y)).is_none_or(plain) {
        end -= 1;
    }
    let mut s = String::new();
    let mut cur: Option<(Color, Color, Modifier)> = None;
    let mut x = 0;
    while x < end {
        let Some(c) = buf.cell((x, y)) else { break };
        let st = (c.fg, c.bg, c.modifier);
        if cur != Some(st) {
            s.push_str(&sgr(st));
            cur = Some(st);
        }
        let sym = if c.symbol().is_empty() { " " } else { c.symbol() };
        s.push_str(sym);
        x += UnicodeWidthStr::width(sym).max(1) as u16;
    }
    s.push_str("\x1b[0m");
    if end < a.width {
        s.push_str("\x1b[K");
    }
    s
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
        session: std::env::var("REMAN_SESSION").unwrap_or_default(),
        items: vec![],
        titles: vec![],
        total: 0,
        label: String::new(),
        loaded: false,
        sel: 0,
        want: 200,
        seq: 0,
        shown_seq: 0,
        details: HashMap::new(),
        fixes: HashMap::new(),
        confirm_forget: None,
        message: None,
        help: false,
        jobs: jtx,
    };
    app.refresh();

    let _cp = Utf8Console::enable();
    let mut out = tty()?;
    enable_raw_mode()?;
    execute!(out, EnterAlternateScreen, cursor::Show)?;
    let _ = out.write_all(b"\x1b[?7l"); // no autowrap while we own the screen
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        if let Ok(mut t) = tty() {
            let _ = t.write_all(b"\x1b[?7h");
            let _ = execute!(t, LeaveAlternateScreen, cursor::Show);
        }
        // the alternate screen hides panics; keep a record
        let _ = std::fs::write(crate::config::home().join("tui-panic.log"), format!("{info}\n{}", std::backtrace::Backtrace::force_capture()));
        prev_hook(info);
    }));
    let mut screen = Screen { out: std::io::BufWriter::with_capacity(1 << 16, tty()?), prev: Vec::new() };
    screen.invalidate();
    let chosen = event_loop(&mut screen, &mut app, &rrx);
    disable_raw_mode()?;
    let _ = out.write_all(b"\x1b[?7h");
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

fn event_loop(screen: &mut Screen, app: &mut App, replies: &Receiver<Reply>) -> Result<Option<String>> {
    let mut dirty = true;
    let mut size = terminal::size().unwrap_or((80, 24));
    loop {
        while let Ok(r) = replies.try_recv() {
            dirty = true;
            match r {
                Reply::Results(r) if r.seq >= app.shown_seq => {
                    app.shown_seq = r.seq;
                    app.total = r.total;
                    app.items = r.items;
                    app.titles = r.titles;
                    app.label = r.label;
                    app.loaded = true;
                    app.sel = app.sel.min(app.visible_items().len().saturating_sub(1));
                }
                Reply::Results(..) => {}
                Reply::Detail(cmd, v) => {
                    app.details.insert(cmd, v);
                }
                Reply::Fix(cmd, v) => {
                    app.fixes.insert(cmd, v);
                }
                Reply::Done("forget", v) => {
                    app.message = Some(format!("forgotten - {} record(s) deleted", v["removed"]));
                    app.refresh();
                }
                Reply::Done(_, _) => app.refresh(),
                Reply::Error(e) => {
                    app.message = Some(e);
                    app.loaded = true;
                }
            }
        }
        if let Some(it) = app.selected() {
            if app.mode != Mode::Flows && !app.details.contains_key(&it.command) {
                app.details.insert(it.command.clone(), Value::Null);
                let _ = app.jobs.send(Job::Other("detail", json!({"op": "detail", "command": it.command})));
            }
            if app.mode == Mode::Fixes && app.query.trim().is_empty() && !app.fixes.contains_key(&it.command) {
                app.fixes.insert(it.command.clone(), json!("pending"));
                let _ = app.jobs.send(Job::Other("fixfor", json!({"op": "fixfor", "command": it.command, "cwd": app.cwd})));
            }
        }
        if dirty {
            let mut buf = Buffer::empty(Rect::new(0, 0, size.0, size.1));
            let cur = draw(&mut buf, app);
            screen.frame(&buf, cur)?;
            dirty = false;
        }
        if !event::poll(Duration::from_millis(16))? {
            continue;
        }
        let ev = event::read()?;
        dirty = true;
        let k = match ev {
            Event::Key(k) => k,
            Event::Resize(w, h) => {
                size = (w, h);
                screen.invalidate();
                continue;
            }
            _ => continue,
        };
        if k.kind != KeyEventKind::Press {
            continue; // Windows reports press AND release
        }
        if app.help {
            app.help = false;
            continue;
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let n = app.visible_items().len();
        if k.code != KeyCode::Delete {
            app.confirm_forget = None;
        }
        match k.code {
            KeyCode::Esc => return Ok(None),
            KeyCode::Char('c') | KeyCode::Char('d') if ctrl => return Ok(None),
            KeyCode::Enter => return Ok(app.pick()),
            KeyCode::F(1) => app.help = true,
            KeyCode::Down => app.sel = (app.sel + 1).min(n.saturating_sub(1)),
            KeyCode::Up => app.sel = app.sel.saturating_sub(1),
            KeyCode::Char('n') if ctrl => app.sel = (app.sel + 1).min(n.saturating_sub(1)),
            KeyCode::PageDown => app.sel = (app.sel + 10).min(n.saturating_sub(1)),
            KeyCode::PageUp => app.sel = app.sel.saturating_sub(10),
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
                app.items.clear();
                app.titles.clear();
                app.loaded = false;
                app.label.clear();
                app.reset();
            }
            KeyCode::Char('g') if ctrl => {
                app.group = !app.group;
                app.reset();
            }
            KeyCode::Char('p') if ctrl => {
                if let Some(it) = app.selected().filter(|_| app.mode != Mode::Flows) {
                    app.message = Some(if it.pinned { "unpinned".into() } else { "pinned - it ranks first from now on".into() });
                    let _ = app.jobs.send(Job::Other("pin", json!({"op": "pin", "command": it.command, "on": !it.pinned})));
                }
            }
            KeyCode::Delete => {
                if let Some(it) = app.selected().filter(|_| app.mode != Mode::Flows) {
                    if app.confirm_forget.as_deref() == Some(it.command.as_str()) {
                        let _ = app.jobs.send(Job::Other("forget", json!({"op": "forget", "command": it.command})));
                        app.confirm_forget = None;
                    } else {
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
        assert_eq!(UnicodeWidthStr::width(fit("日本語テキスト", 6).as_str()), 6);
    }

    #[test]
    fn clean_drops_zero_width() {
        assert_eq!(clean("a\u{200d}b\u{fe0f}c"), "abc");
        assert_eq!(clean("git add .\ngit commit"), "git add . ↵ git commit");
    }

    #[test]
    fn lines_are_painted_whole() {
        let mut b = Buffer::empty(Rect::new(0, 0, 10, 2));
        b.set_string(0, 0, "ab", Style::default().fg(Color::Green));
        let r = row_ansi(&b, 0);
        assert!(r.contains("ab") && r.ends_with("\x1b[0m\x1b[K"), "{r:?}");
        b.set_string(0, 1, "0123456789", Style::default());
        assert!(!row_ansi(&b, 1).ends_with("\x1b[K")); // full line: no EL at the wrap position
    }
}
