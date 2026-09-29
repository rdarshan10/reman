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
use ratatui::crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers};
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
    /// (n, of): a step of a flow - opened in the Flows tab, or the flow you're walking through
    step: Option<(usize, usize)>,
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
    /// type the pick onto the shell's next prompt (Command Prompt's `h` macro): ready to edit, not run
    pub to_prompt: bool,
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
                                    // a flow you're walking through: its next step sits right above the prompt
                                    if let Some(fl) = v.get("flow").filter(|f| f.is_object()) {
                                        let items: Vec<Item> = fl["items"]
                                            .as_array()
                                            .map(|a| {
                                                a.iter()
                                                    .map(|x| {
                                                        let mut it = item_of(x);
                                                        it.step = Some((x["flow_step"].as_u64().unwrap_or(0) as usize, x["flow_total"].as_u64().unwrap_or(0) as usize));
                                                        it
                                                    })
                                                    .collect()
                                            })
                                            .unwrap_or_default();
                                        let done = fl["pos"].as_u64().unwrap_or(0);
                                        let total = fl["steps"].as_array().map(Vec::len).unwrap_or(0);
                                        push(&mut r, format!("flow in progress · {done} of {total} done · ^X stops it"), items);
                                    }
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
                                            title = format!("{title} · {hidden} agent one-off{} hidden (F3 shows)", if hidden == 1 { "" } else { "s" });
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
    /// Flows tab: the flow opened with Enter, shown as its steps
    flow: Option<FlowView>,
    /// where the last frame put the tabs and the rows (mouse clicks)
    hits: Hits,
    /// the other tabs' queries, restored when you switch back
    queries: [String; 3],
    /// F10: close the finder and open `reman settings`
    open_settings: bool,
}

struct FlowView {
    steps: Vec<Item>,
    count: u64,
    /// selection to restore when going back to the list
    back_sel: usize,
}

#[derive(Default, Clone)]
struct Hits {
    tabs_y: u16,
    tabs: Vec<(u16, u16, Mode)>,
    rows: Vec<(u16, usize)>,
}

const MODES: [Mode; 3] = [Mode::Recall, Mode::Fixes, Mode::Flows];

impl App {
    fn set_mode(&mut self, m: Mode) {
        if self.mode == m && self.flow.is_none() {
            return;
        }
        // each tab keeps its own query: "run migrations" means nothing to Fixes
        let slot = |m: Mode| MODES.iter().position(|x| *x == m).unwrap_or(0);
        self.queries[slot(self.mode)] = std::mem::take(&mut self.query);
        self.query = std::mem::take(&mut self.queries[slot(m)]);
        self.mode = m;
        self.flow = None;
        self.items.clear();
        self.titles.clear();
        self.loaded = false;
        self.label.clear();
        self.reset();
    }

    fn cycle_mode(&mut self, fwd: bool) {
        let i = MODES.iter().position(|m| *m == self.mode).unwrap_or(0);
        self.set_mode(MODES[if fwd { (i + 1) % 3 } else { (i + 2) % 3 }]);
    }

    /// Enter on a flow: show its steps (each a real command with its own stats).
    fn open_flow(&mut self, it: &Item) {
        let steps: Vec<Item> = it.meta["sequence"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .enumerate()
                    .map(|(i, s)| {
                        let meta = self.details.get(s).filter(|d| d.is_object()).cloned().unwrap_or_else(|| json!({"command": s}));
                        let mut x = item_of(&meta);
                        x.command = s.to_string();
                        x.step = Some((i + 1, a.len()));
                        x
                    })
                    .collect()
            })
            .unwrap_or_default();
        for s in &steps {
            if !self.details.contains_key(&s.command) {
                self.details.insert(s.command.clone(), Value::Null);
                let _ = self.jobs.send(Job::Other("detail", json!({"op": "detail", "command": s.command})));
            }
        }
        self.flow = Some(FlowView { steps, count: it.meta["count"].as_u64().unwrap_or(0), back_sel: self.sel });
        self.sel = 0;
    }

    fn close_flow(&mut self) -> bool {
        match self.flow.take() {
            Some(f) => {
                self.sel = f.back_sel;
                true
            }
            None => false,
        }
    }

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
        if let Some(f) = &self.flow {
            return f.steps.iter().collect();
        }
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
pub(crate) fn clean(s: &str) -> String {
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
pub(crate) fn fit(s: &str, w: usize) -> String {
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
pub(crate) const ACCENT: Color = Color::Blue;
pub(crate) const OK: Color = Color::Green;
pub(crate) const BAD: Color = Color::Red;
pub(crate) const WARN: Color = Color::Yellow;
pub(crate) const INFO: Color = Color::Cyan;
pub(crate) const MUTED: Color = Color::DarkGray;

pub(crate) fn muted() -> Style {
    Style::default().fg(MUTED)
}

fn glyph(it: &Item, walking: bool) -> (&'static str, Color) {
    if it.fix {
        return ("→", OK);
    }
    if walking {
        return ("⇢", INFO);
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

/// Bottom-up, like the prompt it replaces: the query is the last line, the filters sit right
/// above it, and the best result is nearest to both. ↑ moves further back.
fn draw(buf: &mut Buffer, app: &App) -> ((u16, u16), Hits) {
    let area = buf.area;
    let (w, h) = (area.width as usize, area.height);
    let mut hits = Hits::default();
    if w < 24 || h < 7 {
        Paragraph::new("reman: terminal too small").render(area, buf);
        return ((0, 0), hits);
    }
    let items = app.visible_items();
    let input_y = h - 1;
    let filter_y = h - 2;

    // last line: the query, the tabs on the right
    let tabs = [(Mode::Recall, "Recall"), (Mode::Fixes, "Fixes"), (Mode::Flows, "Flows")];
    let tabs_w: usize = tabs.iter().map(|t| t.1.len() + 2).sum::<usize>() + 1;
    let prompt = if app.flow.is_some() { " ⇢ " } else { " › " };
    let pw = UnicodeWidthStr::width(prompt);
    let qw = w.saturating_sub(pw + tabs_w + 1);
    let shown_q: String = {
        let n = app.query.chars().count();
        if n > qw { app.query.chars().skip(n - qw).collect() } else { app.query.clone() }
    };
    let mut row = vec![Span::styled(prompt, Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))];
    if app.query.is_empty() {
        let hint = match (app.mode, app.flow.is_some()) {
            (_, true) => "↵ runs the selected step and queues the rest",
            (Mode::Recall, _) => "describe it in words, or type part of the command",
            (Mode::Fixes, _) => "type the command that failed",
            (Mode::Flows, _) => "filter flows · ↵ opens one",
        };
        row.push(Span::styled(fit(hint, qw), muted()));
    } else {
        row.push(Span::styled(fit(&shown_q, qw), Style::default().add_modifier(Modifier::BOLD)));
    }
    row.push(Span::raw(" "));
    let mut x = (pw + qw + 1) as u16;
    for (m, name) in tabs {
        let st = if app.mode == m { Style::default().fg(ACCENT).add_modifier(Modifier::BOLD | Modifier::UNDERLINED) } else { muted() };
        let label = format!(" {name} ");
        let lw = label.len() as u16;
        hits.tabs.push((x, x + lw, m));
        x += lw;
        row.push(Span::styled(label, st));
    }
    hits.tabs_y = input_y;
    Paragraph::new(Line::from(row)).render(Rect::new(0, input_y, area.width, 1), buf);
    let cursor = ((pw + UnicodeWidthStr::width(shown_q.as_str())).min(w - 1) as u16, input_y);

    // the line above: filters as a sentence (or the question / message of the moment)
    let line = if let Some(c) = &app.confirm_forget {
        Line::from(vec![Span::styled(format!(" Del again forgets `{}` everywhere", one_line(c)), Style::default().fg(BAD).add_modifier(Modifier::BOLD)), Span::styled("  ·  any other key keeps it", muted())])
    } else if let Some(m) = &app.message {
        Line::from(Span::styled(format!(" {m}"), Style::default().fg(WARN)))
    } else {
        filter_line(app, &items, w)
    };
    Paragraph::new(line).render(Rect::new(0, filter_y, area.width, 1), buf);

    // first line: the keys that matter right now
    Paragraph::new(hint_line(app)).render(Rect::new(0, 0, area.width, 1), buf);

    // body: list + card (right column when wide, above the list when narrow)
    let body = Rect::new(0, 1, area.width, filter_y - 1);
    let wide = w >= 118 && h >= 12;
    let (list, card) = if wide {
        let cw = (w as u16 * 2 / 5).clamp(40, 64);
        (Rect::new(0, body.y, area.width - cw, body.height), Rect::new(area.width - cw, body.y, cw, body.height))
    } else {
        let ch = if h >= 20 { 6 } else if h >= 12 { 4 } else { 0 };
        (Rect::new(0, body.y + ch, area.width, body.height.saturating_sub(ch)), Rect::new(0, body.y, area.width, ch))
    };
    draw_list(buf, app, &items, list, &mut hits);
    draw_card(buf, app, &items, card, wide);
    if app.help {
        draw_help(buf, area);
    }
    (cursor, hits)
}

fn filter_line<'a>(app: &App, items: &[&Item], w: usize) -> Line<'a> {
    let val = |on: bool, s: &str| Span::styled(s.to_string(), if on { Style::default().fg(ACCENT) } else { Style::default() });
    let key = |k: &str| Span::styled(format!(" {k}"), muted());
    let dot = || Span::styled("  ·  ", muted());
    let mut row = vec![Span::raw("   ")];
    let right;
    if let Some(f) = &app.flow {
        row.push(Span::styled(format!("a flow you ran {}x · {} steps", f.count, f.steps.len()), Style::default().fg(INFO)));
        right = format!("step {} of {} ", app.sel + 1, f.steps.len());
    } else {
        // with a query, folder/repo scope means "these first", never "only these"
        let scope_txt = match (app.mode == Mode::Recall && !app.query.trim().is_empty(), app.scope) {
            (true, ScopeSel::Folder) => "this folder first",
            (true, ScopeSel::Repo) => "this repo first",
            (_, s) => s.phrase(),
        };
        row.extend([val(app.scope != ScopeSel::Folder, scope_txt), key("←→")]);
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
            row.extend([dot(), val(app.actor != "all", actor), key("F3"), dot(), val(app.status != "all", status), key("F2")]);
            if !app.query.trim().is_empty() {
                row.extend([dot(), val(!app.group, if app.group { "variants folded" } else { "every variant" }), key("^G")]);
            }
        }
        right = if !app.loaded {
            "… ".to_string()
        } else if app.total > items.len() {
            format!("{} of {} ", items.len(), app.total)
        } else if items.len() == 1 {
            "1 result ".to_string()
        } else {
            format!("{} results ", items.len())
        };
    }
    let used: usize = row.iter().map(|s| UnicodeWidthStr::width(s.content.as_ref())).sum();
    if used + right.len() + 1 < w {
        row.push(Span::raw(" ".repeat(w - used - right.len())));
        row.push(Span::styled(right, muted()));
    }
    Line::from(row)
}

fn hint_line<'a>(app: &App) -> Line<'a> {
    let k: Vec<(&str, &str)> = if app.flow.is_some() {
        vec![("↵", "run this step, queue the rest"), ("^A", "insert all steps"), ("↑↓", "move"), ("←", "back to flows")]
    } else if app.mode == Mode::Flows {
        vec![("↵", "open flow"), ("^A", "insert all steps"), ("↑↓", "move"), ("tab", "next tab"), ("F1", "keys"), ("esc", "close")]
    } else {
        let enter = if app.selected().is_some_and(|it| app.fix_of(&it).is_some()) { "insert the fix" } else { "insert" };
        vec![("↵", enter), ("↑↓", "move"), ("tab", "Recall · Fixes · Flows"), ("Del", "forget"), ("^P", "pin"), ("F1", "keys"), ("esc", "close")]
    };
    let mut sp = vec![Span::raw(" ")];
    for (key, what) in k {
        sp.push(Span::styled(key.to_string(), Style::default().add_modifier(Modifier::BOLD)));
        sp.push(Span::styled(format!(" {what}   "), muted()));
    }
    Line::from(sp)
}

/// List lines from the BOTTOM up, each with the item it shows. A section's heading sits at the
/// base of its stack (you read upward from the prompt). Returns (lines, line of the selection).
fn list_rows<'a>(app: &App, items: &[&'a Item], w: usize) -> (Vec<(Line<'a>, Option<usize>)>, usize) {
    let walking = |it: &Item| it.step.is_some() && app.flow.is_none();
    let headers = app.flow.is_some() || app.titles.len() > 1 || app.mode != Mode::Recall || items.first().is_some_and(|i| i.fix || i.predicted || walking(i));
    let mut lines: Vec<(Line, Option<usize>)> = Vec::new();
    let mut sel_row = 0;
    let mut cur: Option<usize> = None;
    let meta_w = if w >= 70 { 22 } else if w >= 50 { 12 } else { 0 };
    let cmd_w = w.saturating_sub(4 + meta_w + 1);
    for (idx, it) in items.iter().enumerate() {
        if headers && cur != Some(it.section) {
            cur = Some(it.section);
            let t = match &app.flow {
                Some(f) => format!("steps · ran together {}x · ↵ runs from the selected one", f.count),
                None => app.titles.get(it.section).cloned().unwrap_or_default(),
            };
            let t = format!("   {t} ");
            let rule = "─".repeat(w.saturating_sub(UnicodeWidthStr::width(t.as_str()) + 1));
            if !lines.is_empty() {
                lines.push((Line::raw(""), None));
            }
            lines.push((Line::from(vec![Span::styled(t, Style::default().fg(MUTED).add_modifier(Modifier::BOLD)), Span::styled(rule, muted())]), None));
        }
        let sel = idx == app.sel;
        if sel {
            sel_row = lines.len();
        }
        let (g, gc) = glyph(it, walking(it));
        let text_st = if sel { Style::default().add_modifier(Modifier::BOLD) } else { Style::default() };
        let hi = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);
        let mut row = vec![
            Span::styled(if sel { " ▌" } else { "  " }, Style::default().fg(ACCENT)),
            Span::styled(g, Style::default().fg(gc)),
            Span::styled(if it.pinned { "*" } else { " " }, Style::default().fg(WARN)),
        ];
        let shown = match (app.flow.is_some(), it.step) {
            (true, Some((n, _))) => format!("{n}. {}", it.command),
            _ if app.mode == Mode::Flows && app.flow.is_none() => it.command.replace(" ; ", "  →  "),
            _ => it.command.clone(),
        };
        for (seg, m) in highlight(&fit(&clean(&shown), cmd_w), &app.query) {
            row.push(Span::styled(seg, if m { hi } else { text_st }));
        }
        if meta_w > 0 {
            row.push(Span::raw(" "));
            let m = &it.meta;
            if it.fix || it.predicted || walking(it) || (app.mode == Mode::Flows && app.flow.is_none()) {
                let note = if it.fix {
                    if m["proven"].as_bool() == Some(true) { "worked last time".to_string() } else { "closest you ran".to_string() }
                } else if let (true, Some((n, of))) = (walking(it), it.step) {
                    format!("step {n} of {of}")
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
                    row.push(Span::styled(format!("{:>5} {:>4}  ", format!("{runs}×"), age), muted()));
                    let wc = if it.actor.starts_with("agent") { WARN } else { MUTED };
                    row.push(Span::styled(fit(&who(&it.actor), 7), Style::default().fg(wc)));
                } else {
                    row.push(Span::styled(fit(&format!("{:>5} {:>4}", format!("{runs}×"), age), meta_w), muted()));
                }
            }
        }
        lines.push((Line::from(row), Some(idx)));
    }
    (lines, sel_row)
}

fn draw_list(buf: &mut Buffer, app: &App, items: &[&Item], r: Rect, hits: &mut Hits) {
    let h = r.height as usize;
    if h == 0 {
        return;
    }
    let bottom = r.y + r.height - 1;
    if items.is_empty() {
        let (a, b) = empty_state(app);
        buf.set_string(r.x + 3, bottom.saturating_sub(1), fit(&a, r.width.saturating_sub(4) as usize), Style::default());
        buf.set_string(r.x + 3, bottom, fit(&b, r.width.saturating_sub(4) as usize), muted());
        return;
    }
    let (lines, sel_row) = list_rows(app, items, r.width as usize);
    // keep the selection in view with a line of context above it
    let start = if sel_row + 2 > h { sel_row + 2 - h } else { 0 };
    let start = start.min(lines.len().saturating_sub(h));
    for (i, (line, idx)) in lines.into_iter().skip(start).take(h).enumerate() {
        let y = bottom - i as u16;
        buf.set_line(r.x, y, &line, r.width);
        if let Some(ix) = idx {
            hits.rows.push((y, ix));
        }
    }
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
            if app.actor != "all" || app.status != "all" { "Filters are on: F3 / F2 to widen them.".into() } else { "Try other words, or tab for Fixes and Flows.".into() },
        ),
        Mode::Fixes if q.is_empty() => ("No failed commands recorded here.".into(), "←→ to look in other folders.".into()),
        Mode::Fixes => (format!("Nothing you ran looks like a fix for \"{q}\"."), "Fixes are learned when a failed command is followed by one that works.".into()),
        Mode::Flows => ("No repeated sequences yet.".into(), "A flow appears once you run the same steps a few times.".into()),
    }
}

fn draw_card(buf: &mut Buffer, app: &App, items: &[&Item], r: Rect, wide: bool) {
    if r.height < 2 {
        return;
    }
    // separator: a left rule beside the list, a bottom rule above it
    let inner = if wide {
        for y in r.y..r.y + r.height {
            buf.set_string(r.x, y, "│", muted());
        }
        Rect::new(r.x + 2, r.y, r.width.saturating_sub(3), r.height)
    } else {
        buf.set_string(r.x, r.y + r.height - 1, "─".repeat(r.width as usize), muted());
        Rect::new(r.x + 1, r.y, r.width.saturating_sub(2), r.height - 1)
    };
    let Some(it) = items.get(app.sel) else { return };
    let m = &it.meta;
    let mut l: Vec<Line> = Vec::new();
    let body = Style::default();
    if app.mode == Mode::Flows && app.flow.is_none() {
        l.push(Line::styled(format!("You ran these {} steps together {} times", m["length"], m["count"]), Style::default().fg(INFO)));
        let steps: Vec<String> = m["sequence"].as_array().map(|a| a.iter().filter_map(Value::as_str).map(one_line).collect()).unwrap_or_default();
        for (i, s) in steps.iter().enumerate() {
            l.push(Line::from(vec![Span::styled(format!("{}. ", i + 1), muted()), Span::styled(s.clone(), body)]));
        }
        l.push(Line::styled("↵ to walk through it: each step lands on your prompt, the next one waits on ↑", muted()));
    } else {
        if wide {
            l.push(Line::styled(clean(&it.command), Style::default().add_modifier(Modifier::BOLD)));
        }
        // a step of a flow: where it sits and what follows
        if let Some((n, of)) = it.step {
            let steps: Vec<&Item> = match &app.flow {
                Some(f) => f.steps.iter().collect(),
                None => items.iter().copied().filter(|x| x.step.is_some()).collect(),
            };
            let after = steps.iter().find(|x| x.step.map(|s| s.0) == Some(n + 1)).map(|x| one_line(&x.command));
            let txt = match (&app.flow, after) {
                (Some(_), Some(a)) => format!("Step {n} of {of}. ↵ puts it on your prompt; `{a}` then waits on ↑."),
                (Some(_), None) => format!("Step {n} of {of}, the last one."),
                (None, Some(a)) => format!("Next in the flow you started (step {n} of {of}); then `{a}`."),
                (None, None) => format!("The last step of the flow you started ({n} of {of})."),
            };
            l.push(Line::styled(txt, Style::default().fg(INFO)));
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
        if runs > 0 {
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
        }
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
        ("↑ ↓  PgUp PgDn", "move (↑ goes further back)"),
        ("tab  shift-tab", "next / previous: Recall → Fixes → Flows"),
        ("alt-1  2  3", "jump straight to Recall / Fixes / Flows"),
        ("← →", "where: this folder → this repo → everywhere"),
        ("F3", "who: anyone → you → agents"),
        ("F2", "outcome: any → worked → failed"),
        ("^G", "fold variants of a command / show each"),
        ("^P", "pin: pinned commands rank first"),
        ("Del Del", "forget a command everywhere (e.g. it held a secret)"),
        ("Flows ↵", "open a flow; ↵ on a step runs from there, the rest queue"),
        ("Flows ^A", "insert every step as one line"),
        ("^X", "stop the flow in progress"),
        ("^U  ^W", "clear the query / delete a word"),
        ("mouse", "click a tab or a row, wheel to scroll"),
        ("F10", "settings: coding tools, shared folders, privacy"),
        ("esc", "back / close"),
    ];
    let bw = (area.width as usize).min(78) as u16;
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
pub(crate) struct Screen {
    pub(crate) out: std::io::BufWriter<std::fs::File>,
    pub(crate) prev: Vec<String>,
}

impl Screen {
    pub(crate) fn invalidate(&mut self) {
        self.prev.clear();
        let _ = self.out.write_all(b"\x1b[0m\x1b[2J");
    }

    pub(crate) fn frame(&mut self, buf: &Buffer, cursor: (u16, u16)) -> std::io::Result<()> {
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
pub(crate) fn tty() -> Result<std::fs::File> {
    let path = if cfg!(windows) { "CONOUT$" } else { "/dev/tty" };
    Ok(std::fs::OpenOptions::new().read(true).write(true).open(path)?)
}

/// Console output code page -> UTF-8 while the finder is up (the frame is UTF-8; a legacy OEM
/// code page turns `»` into `Γ├`), restored on drop.
pub(crate) struct Utf8Console(#[allow(dead_code)] u32);

impl Utf8Console {
    pub(crate) fn enable() -> Self {
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
    pub const KEY_EVENT: u16 = 1;

    /// KEY_EVENT_RECORD (16 bytes)
    #[repr(C)]
    pub struct KeyEvent {
        pub key_down: i32,
        pub repeat: u16,
        pub vk: u16,
        pub scan: u16,
        pub ch: u16,
        pub ctrl: u32,
    }

    /// INPUT_RECORD holding a key event (20 bytes)
    #[repr(C)]
    pub struct InputRecord {
        pub event_type: u16,
        pub _pad: u16,
        pub key: KeyEvent,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        pub fn GetConsoleOutputCP() -> u32;
        pub fn SetConsoleOutputCP(cp: u32) -> i32;
        pub fn WriteConsoleInputW(input: *mut core::ffi::c_void, buf: *const InputRecord, len: u32, written: *mut u32) -> i32;
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
        flow: None,
        hits: Hits::default(),
        queries: Default::default(),
        open_settings: false,
    };
    app.refresh();

    let _cp = Utf8Console::enable();
    let mut out = tty()?;
    enable_raw_mode()?;
    execute!(out, EnterAlternateScreen, EnableMouseCapture, cursor::Show)?;
    let _ = out.write_all(b"\x1b[?7l"); // no autowrap while we own the screen
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        if let Ok(mut t) = tty() {
            let _ = t.write_all(b"\x1b[?7h");
            let _ = execute!(t, DisableMouseCapture, LeaveAlternateScreen, cursor::Show);
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
    execute!(out, DisableMouseCapture, LeaveAlternateScreen, cursor::Show)?;
    let chosen = chosen?;
    if app.open_settings {
        return crate::settings_ui::run();
    }
    if let Some(c) = chosen {
        match o.result_file {
            Some(p) => std::fs::write(p, c)?,
            None if o.to_prompt => type_ahead(&c)?,
            None => println!("{c}"),
        }
    }
    Ok(())
}

/// Queue `text` as typed keys in this console's input buffer, so the shell's next prompt starts
/// with it - ready to edit, not run (Command Prompt reads it like anything you type ahead).
/// Newlines would run a multi-line command early, so they become spaces.
#[cfg(windows)]
fn type_ahead(text: &str) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    let con = std::fs::OpenOptions::new().read(true).write(true).open("CONIN$")?;
    let units: Vec<u16> = text.replace(['\r', '\n'], " ").encode_utf16().collect();
    let mut recs: Vec<win::InputRecord> = Vec::with_capacity(units.len() * 2);
    for ch in units {
        for down in [1, 0] {
            recs.push(win::InputRecord {
                event_type: win::KEY_EVENT,
                _pad: 0,
                key: win::KeyEvent { key_down: down, repeat: 1, vk: 0, scan: 0, ch, ctrl: 0 },
            });
        }
    }
    let mut written = 0u32;
    let ok = unsafe { win::WriteConsoleInputW(con.as_raw_handle(), recs.as_ptr(), recs.len() as u32, &mut written) };
    if ok == 0 {
        anyhow::bail!("could not type the pick onto the prompt: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(windows))]
fn type_ahead(text: &str) -> Result<()> {
    println!("{text}");
    Ok(())
}

/// Start walking through a flow from step `pos`: the daemon remembers it for this shell session,
/// and every later ↑ offers the next step first. Synchronous - the finder is about to exit.
fn arm_flow(app: &App, steps: &[String], pos: usize) {
    if !app.session.is_empty() && pos + 1 < steps.len() {
        let _ = crate::client::call(&json!({"op": "flow_arm", "session": app.session, "steps": steps, "pos": pos}));
    }
}

enum Act {
    None,
    Pick(Option<String>),
    Quit,
}

/// Enter (or a click on the selected row).
fn enter(app: &mut App) -> Act {
    if let Some(f) = &app.flow {
        let steps: Vec<String> = f.steps.iter().map(|s| s.command.clone()).collect();
        let Some(step) = steps.get(app.sel).cloned() else { return Act::None };
        arm_flow(app, &steps, app.sel);
        return Act::Pick(Some(step));
    }
    if app.mode == Mode::Flows {
        if let Some(it) = app.selected() {
            app.open_flow(&it);
        }
        return Act::None;
    }
    Act::Pick(app.pick())
}

fn move_sel(app: &mut App, by: isize) {
    let n = app.visible_items().len();
    app.sel = (app.sel as isize + by).clamp(0, n.saturating_sub(1) as isize) as usize;
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
                    // an open flow's steps show each command's own stats once they arrive
                    if let Some(f) = app.flow.as_mut() {
                        for s in f.steps.iter_mut().filter(|s| s.command == cmd && v.is_object()) {
                            let step = s.step;
                            *s = item_of(&v);
                            s.command = cmd.clone();
                            s.step = step;
                        }
                    }
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
            if (app.mode != Mode::Flows || app.flow.is_some()) && !app.details.contains_key(&it.command) {
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
            let (cur, hits) = draw(&mut buf, app);
            app.hits = hits;
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
            Event::Mouse(me) => {
                use ratatui::crossterm::event::{MouseButton, MouseEventKind};
                if app.help {
                    app.help = false;
                    continue;
                }
                match me.kind {
                    // bottom-up: the wheel moves the way the list scrolls
                    MouseEventKind::ScrollUp => move_sel(app, 1),
                    MouseEventKind::ScrollDown => move_sel(app, -1),
                    MouseEventKind::Down(MouseButton::Left) => {
                        if me.row == app.hits.tabs_y {
                            if let Some(&(_, _, m)) = app.hits.tabs.iter().find(|(a, b, _)| me.column >= *a && me.column < *b) {
                                app.set_mode(m);
                            }
                        } else if let Some(&(_, idx)) = app.hits.rows.iter().find(|(y, _)| *y == me.row) {
                            if idx == app.sel {
                                // a click on the selected row acts like Enter
                                match enter(app) {
                                    Act::Pick(p) => return Ok(p),
                                    Act::Quit => return Ok(None),
                                    Act::None => {}
                                }
                            } else {
                                app.sel = idx;
                            }
                        }
                    }
                    _ => {}
                }
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
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        if k.code != KeyCode::Delete {
            app.confirm_forget = None;
        }
        let act = match k.code {
            KeyCode::Esc => {
                if app.close_flow() { Act::None } else { Act::Quit }
            }
            KeyCode::Char('c') | KeyCode::Char('d') if ctrl => Act::Quit,
            KeyCode::Enter => enter(app),
            KeyCode::F(1) => {
                app.help = true;
                Act::None
            }
            KeyCode::F(10) => {
                app.open_settings = true;
                Act::Quit
            }
            // bottom-up: ↑ goes further back, ↓ comes toward the prompt
            KeyCode::Up => {
                move_sel(app, 1);
                Act::None
            }
            KeyCode::Down => {
                move_sel(app, -1);
                Act::None
            }
            KeyCode::PageUp => {
                move_sel(app, 10);
                Act::None
            }
            KeyCode::PageDown => {
                move_sel(app, -10);
                Act::None
            }
            KeyCode::Tab => {
                app.cycle_mode(true);
                Act::None
            }
            KeyCode::BackTab => {
                app.cycle_mode(false);
                Act::None
            }
            KeyCode::Char('t') if ctrl => {
                app.cycle_mode(true);
                Act::None
            }
            KeyCode::Char(c @ '1'..='3') if alt => {
                app.set_mode(MODES[(c as u8 - b'1') as usize]);
                Act::None
            }
            KeyCode::Left if app.flow.is_some() => {
                app.close_flow();
                Act::None
            }
            KeyCode::Left | KeyCode::Right => {
                let fwd = k.code == KeyCode::Right;
                app.scope = match (app.scope, fwd) {
                    (ScopeSel::Folder, true) | (ScopeSel::All, false) => ScopeSel::Repo,
                    (ScopeSel::Repo, true) | (ScopeSel::Folder, false) => ScopeSel::All,
                    _ => ScopeSel::Folder,
                };
                app.reset();
                Act::None
            }
            KeyCode::F(3) => {
                app.actor = match app.actor {
                    "all" => "you",
                    "you" => "agent",
                    _ => "all",
                };
                app.reset();
                Act::None
            }
            KeyCode::F(2) => {
                app.status = match app.status {
                    "all" => "ok",
                    "ok" => "fail",
                    _ => "all",
                };
                app.reset();
                Act::None
            }
            KeyCode::Char('a') if ctrl => {
                // every step of the flow, as one line
                let steps: Vec<String> = match &app.flow {
                    Some(f) => f.steps.iter().map(|s| s.command.clone()).collect(),
                    None if app.mode == Mode::Flows => app.selected().and_then(|it| it.meta["sequence"].as_array().cloned()).unwrap_or_default().iter().filter_map(Value::as_str).map(String::from).collect(),
                    None => vec![],
                };
                if steps.is_empty() { Act::None } else { Act::Pick(Some(steps.join("; "))) }
            }
            KeyCode::Char('x') if ctrl => {
                if !app.session.is_empty() {
                    let r = crate::client::call(&json!({"op": "flow_stop", "session": app.session}));
                    app.message = Some(if r.is_ok_and(|v| v["stopped"] == json!(true)) { "flow stopped".into() } else { "no flow in progress".into() });
                    app.reset();
                }
                Act::None
            }
            KeyCode::Char('g') if ctrl => {
                app.group = !app.group;
                app.reset();
                Act::None
            }
            KeyCode::Char('p') if ctrl => {
                if let Some(it) = app.selected().filter(|_| app.mode != Mode::Flows || app.flow.is_some()) {
                    app.message = Some(if it.pinned { "unpinned".into() } else { "pinned - it ranks first from now on".into() });
                    let _ = app.jobs.send(Job::Other("pin", json!({"op": "pin", "command": it.command, "on": !it.pinned})));
                }
                Act::None
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
                Act::None
            }
            KeyCode::Char('u') if ctrl => {
                app.close_flow();
                app.query.clear();
                app.reset();
                Act::None
            }
            KeyCode::Char('w') if ctrl => {
                let t = app.query.trim_end().to_string();
                app.query = t.rfind(' ').map(|i| t[..=i].to_string()).unwrap_or_default();
                app.reset();
                Act::None
            }
            KeyCode::Backspace => {
                if app.query.is_empty() {
                    app.close_flow();
                } else {
                    app.close_flow();
                    app.query.pop();
                    app.reset();
                }
                Act::None
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                app.close_flow();
                app.query.push(c);
                app.message = None;
                if app.mode == Mode::Flows {
                    app.sel = 0; // flows filter locally
                } else {
                    app.reset();
                }
                Act::None
            }
            _ => Act::None,
        };
        match act {
            Act::Pick(p) => return Ok(p),
            Act::Quit => return Ok(None),
            Act::None => {}
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
