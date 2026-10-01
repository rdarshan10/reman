//! The warm daemon: model loaded once, whole history in memory, JSON-lines over localhost TCP.
//! Wire-compatible with the Python daemon (same ops/fields on 127.0.0.1:8765), so old clients
//! (the PowerShell hook, reman_hook_claude.py) keep working during cutover. New: persistent
//! connections (many requests per socket), paging, hybrid ranking, fix-pairs, prediction,
//! spool draining, forget/pin, and agent (MCP) tools served from memory.
use crate::config;
use crate::db::{self, Run};
use crate::describe;
use crate::dym;
use crate::embed::Embedder;
use crate::fixpairs::{self, Tracker};
use crate::flows;
use crate::import;
use crate::insight;
use crate::mcp;
use crate::predict;
use crate::search::{self, Query, Rank};
use crate::store::{Scope, Store};
use anyhow::{Context, Result};
use parking_lot::{Mutex, RwLock};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Instant;

pub struct Daemon {
    pub store: RwLock<Store>,
    pub fixes: Mutex<Tracker>,
    pub db: Mutex<Connection>,
    pub embedder: Embedder,
    started: Instant,
    /// session -> a flow you're walking through (picked in the finder's Flows tab)
    flows_armed: Mutex<std::collections::HashMap<String, ArmedFlow>>,
    /// (session, folder) pairs already given their "last time here" line
    greeted: Mutex<std::collections::HashSet<(String, u32)>>,
    /// an agent's command id -> when it started (PreToolUse), for its duration
    agent_starts: Mutex<std::collections::HashMap<String, i64>>,
    /// what is never recorded, from config.json (re-read when the file changes)
    privacy: Mutex<Privacy>,
}

/// What is never recorded as-is: commands and folders matching the ignore patterns, and
/// secrets (masked, or dropped when `"secrets": "drop"` or when the value can't be located).
#[derive(Default)]
struct Privacy {
    stamp: Option<std::time::SystemTime>,
    drop_secrets: bool,
    keep_secrets: bool,
    commands: Vec<regex::Regex>,
    folders: Vec<regex::Regex>,
    /// a desktop notification for a command of yours that ran this many seconds (None = never)
    done_after: Option<u64>,
}

impl Privacy {
    fn refresh(&mut self) {
        let stamp = std::fs::metadata(crate::settings::path()).and_then(|m| m.modified()).ok();
        if stamp.is_some() && stamp == self.stamp {
            return;
        }
        let s = crate::settings::load();
        let compile = |pats: &[String]| -> Vec<regex::Regex> {
            pats.iter()
                .filter_map(|p| match regex::Regex::new(p) {
                    Ok(r) => Some(r),
                    Err(e) => {
                        log(&format!("config.json: ignoring the pattern {p:?}: {e}"));
                        None
                    }
                })
                .collect()
        };
        *self = Privacy {
            stamp,
            drop_secrets: s.drop_secrets(),
            keep_secrets: s.keep_secrets(),
            commands: compile(&s.ignore_commands),
            folders: compile(&s.ignore_folders),
            done_after: s.done_alert_after(),
        };
    }

    /// The run as it may be stored: None when it must not be recorded at all.
    fn admit(&self, mut r: Run) -> Option<Run> {
        if self.commands.iter().any(|x| x.is_match(&r.cmd)) || r.cwd.as_deref().is_some_and(|c| self.folders.iter().any(|x| x.is_match(c))) {
            return None;
        }
        r.err = r.err.and_then(|e| crate::errors::clean_as(&e, !self.keep_secrets));
        if self.keep_secrets {
            return Some(r);
        }
        let masked = crate::redact::redact(&r.cmd);
        let secret = masked != r.cmd;
        if crate::redact::residual_secret(&masked) || (secret && self.drop_secrets) {
            return None;
        }
        r.cmd = masked;
        Some(r)
    }
}

/// A flow being played back: `pos` is the next step to run. Running that step (from the same
/// shell session) advances it; the finder offers `steps[pos]` first until the flow is done.
#[derive(Clone)]
pub struct ArmedFlow {
    pub steps: Vec<String>,
    pub pos: usize,
    pub touched: i64,
}

const FLOW_IDLE_S: i64 = 30 * 60;

impl ArmedFlow {
    /// Advance past `cmd` if it is the next step (or a later one - you may skip ahead).
    fn observe(&mut self, cmd: &str, ok: bool, now: i64) {
        let cmd = cmd.trim();
        if let Some(i) = self.steps.iter().skip(self.pos).position(|s| s.trim() == cmd) {
            if ok {
                self.pos += i + 1;
            }
            self.touched = now;
        }
    }
}

/// Bumped whenever describe() or tldr_map.json changes what commands are described as; a daemon
/// on an older db re-describes the affected commands once, in the background (`redescribe`).
const DESCRIBE_VERSION: &str = "4";

pub fn log(msg: &str) {
    let line = format!("[{}] {msg}\n", config::now());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(config::log_path()) {
        let _ = f.write_all(line.as_bytes());
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn s<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str).filter(|x| !x.is_empty())
}

fn n(v: &Value, k: &str, d: i64) -> i64 {
    v.get(k).and_then(Value::as_i64).unwrap_or(d)
}

fn b(v: &Value, k: &str, d: bool) -> bool {
    v.get(k).and_then(Value::as_bool).unwrap_or(d)
}

/// Parse one ingest-shaped object into a Run.
pub fn run_from(v: &Value, default_actor: &str) -> Option<Run> {
    let cmd = s(v, "command")?.trim().to_string();
    if is_junk(&cmd) {
        return None;
    }
    Some(Run {
        cmd,
        exit: v.get("exit").and_then(Value::as_i64).map(exit_code),
        cwd: s(v, "cwd").map(str::to_string),
        session: s(v, "session").unwrap_or("").to_string(),
        actor: s(v, "actor").unwrap_or(default_actor).to_string(),
        ts: v.get("ts").and_then(Value::as_i64).filter(|t| *t > 0).unwrap_or_else(config::now),
        // a duration past 30 days is a broken clock reading (a start time never set), not a run
        duration_ms: v.get("duration_ms").and_then(Value::as_i64).filter(|d| (0..=30 * 86_400_000).contains(d)),
        // cleaned (and redacted, unless the user keeps secrets) by Privacy::admit
        err: s(v, "error").map(str::to_string),
        // what an agent's hook read from the output; or the output, to read here (never stored)
        reads: v.get("reads").and_then(Value::as_array).map(|a| a.iter().map(Value::as_bool).collect()),
        // (empty is an answer too: a silent tool printing nothing passed)
        output: v.get("output").and_then(Value::as_str).map(str::to_string),
        seen: None,
        branch: None,
        head: None,
    })
}

/// The branch and commit each run's folder has checked out, read now: a run that only just
/// finished (an imported or long-spooled one may have run on another).
fn with_git(mut runs: Vec<Run>) -> Vec<Run> {
    const FRESH_S: i64 = 300;
    let now = config::now();
    let mut seen: std::collections::HashMap<String, crate::git::State> = std::collections::HashMap::new();
    for r in &mut runs {
        let Some(cwd) = r.cwd.as_deref().filter(|_| r.session != "import" && (now - r.ts).abs() <= FRESH_S) else { continue };
        let g = seen.entry(cwd.to_string()).or_insert_with(|| crate::git::state(cwd));
        r.branch = g.branch.clone();
        r.head = g.head.clone();
    }
    runs
}

/// What a run's output said, as corrections to its record (see verdict.rs); the output is
/// dropped here, never stored. (The reads stay on the run: a twin may judge them again.)
fn with_verdicts(mut r: Run) -> Run {
    if r.reads.is_none() {
        r.reads = r.output.as_deref().map(|o| crate::verdict::read(&r.cmd, o));
    }
    r.output = None;
    r.seen = r.reads.as_ref().and_then(|x| seen_json(&r.cmd, r.exit, x));
    r
}

/// Corrections as stored: `[[command, runs, ok, fail], ...]`, or None when there are none.
fn seen_json(cmd: &str, exit: Option<i64>, reads: &[Option<bool>]) -> Option<String> {
    let adj = crate::verdict::adjust(cmd, exit, reads);
    (!adj.is_empty()).then(|| json!(adj.iter().map(|a| json!([a.cmd, a.runs, a.ok, a.fail])).collect::<Vec<_>>()).to_string())
}

/// An exit code as reported. A live report never uses a negative one for "unknown" (that's no
/// code at all): on Windows it is a real failure, its 32-bit code read as signed, like Node's
/// -4058 (ENOENT, 0xFFFFF026) or a crash's -1073741819 (0xC0000005). Kept as Windows has it, unsigned, so it
/// counts as the failure it is. (Old Atuin imports' -1 = unknown never come through here.)
fn exit_code(e: i64) -> i64 {
    if e < 0 && e >= i64::from(i32::MIN) { i64::from(e as i32 as u32) } else { e }
}

/// blank, control-char-only (the \x07 beep) or 1-char noise
pub fn is_junk(cmd: &str) -> bool {
    cmd.chars().filter(|c| !c.is_control()).collect::<String>().trim().chars().count() < 2 || terminal_bootstrap(cmd)
}

/// The line an editor's terminal runs when it opens (VS Code's, Cursor's, Windsurf's shell
/// integration): its own, never the user's or an agent's.
fn terminal_bootstrap(cmd: &str) -> bool {
    let l = cmd.to_lowercase();
    ["shellintegration.ps1", "shellintegration-bash.sh", "shellintegration-rc.zsh", "shellintegration.fish", "shellintegration-env.zsh"].iter().any(|s| l.contains(s))
}

impl Daemon {
    pub fn open() -> Result<Self> {
        let path = config::db_path();
        db::backup_if_legacy(&path)?;
        let conn = db::open(&path)?;
        db::migrate(&conn)?;
        let t = Instant::now();
        let store = Store::load(&conn)?;
        let fixes = Tracker::load(&conn)?;
        log(&format!("loaded {} commands / {} runs in {:?}", store.alive_count(), store.execs.len(), t.elapsed()));
        let embedder = Embedder::load()?;
        embedder.embed_query("warmup query to load the model once")?;
        Ok(Self {
            store: RwLock::new(store),
            fixes: Mutex::new(fixes),
            db: Mutex::new(conn),
            embedder,
            started: Instant::now(),
            flows_armed: Mutex::new(std::collections::HashMap::new()),
            greeted: Mutex::new(std::collections::HashSet::new()),
            agent_starts: Mutex::new(std::collections::HashMap::new()),
            privacy: Mutex::new(Privacy::default()),
        })
    }

    /// An agent that types its commands into a real terminal (Copilot in VS Code, Cursor,
    /// Windsurf) gets each one reported twice: by that shell's prompt hook, as a person's run with
    /// the exact exit code, and by the agent's hook. The second report merges into the first: the
    /// run is the agent's, with the shell's exit code and what the output said. Some(run) when
    /// the run is its own.
    fn unpaired(&self, r: Run) -> Option<Run> {
        const WINDOW: i64 = 30;
        let agent_second = db::TERMINAL_AGENTS.contains(&r.actor.as_str());
        if !(agent_second || r.actor == "human") || r.session == "import" {
            return Some(r);
        }
        let others: &[&str] = if agent_second { &["human"] } else { &db::TERMINAL_AGENTS };
        let conn = self.db.lock();
        let Ok(Some(t)) = db::find_twin(&conn, &r.cmd, r.cwd.as_deref(), r.ts, WINDOW, others) else { return Some(r) };
        if agent_second {
            // the shell's exit code; else the agent's, when the shell couldn't tell
            let learned = r.exit.filter(|_| t.exit.is_none());
            let seen = r.reads.as_ref().and_then(|x| seen_json(&r.cmd, t.exit.or(learned), x));
            if db::claim_for_agent(&conn, &t, &r.actor, seen.as_deref()).and_then(|_| db::learn_exit(&conn, &t, learned)).is_err() {
                return Some(r);
            }
            drop(conn);
            let mut st = self.store.write();
            st.claim_for_agent(t.command_id, &r.actor, seen.as_deref());
            st.settle_twin(t.command_id, t.ts, t.exit, learned, None);
        } else {
            if db::settle_twin(&conn, &t, r.exit).is_err() {
                return Some(r);
            }
            drop(conn);
            self.store.write().settle_twin(t.command_id, t.ts, t.exit, r.exit, t.seen.as_deref());
        }
        None
    }

    /// Write path for everything (hooks, spool, imports). Embeds new texts outside any lock,
    /// then one db transaction, then mirrors into memory. Returns (#new texts, per-run suggestion).
    pub fn ingest_runs(&self, runs: &[Run]) -> Result<(usize, Vec<Option<Value>>)> {
        // what may be stored: ignored commands and folders out, secrets masked (or dropped)
        let admitted: Vec<Run> = {
            let mut p = self.privacy.lock();
            p.refresh();
            runs.iter().cloned().filter_map(|r| p.admit(r)).map(with_verdicts).collect()
        };
        // one run reported twice (a shell, and the agent typing into it) is recorded once
        let admitted: Vec<Run> = with_git(admitted.into_iter().filter_map(|r| self.unpaired(r)).collect());
        let runs = &admitted[..];
        if runs.is_empty() {
            return Ok((0, vec![]));
        }
        let mut new_texts: Vec<&str> = {
            let st = self.store.read();
            runs.iter().map(|r| r.cmd.as_str()).filter(|t| !st.knows(t)).collect()
        };
        new_texts.sort_unstable();
        new_texts.dedup();
        // describe + embed [texts..., desc parts...] in one batched call
        let descs: Vec<describe::Description> = new_texts.iter().map(|t| describe::describe(t)).collect();
        let mut batch: Vec<&str> = new_texts.clone();
        for d in &descs {
            batch.extend(d.parts.iter().map(|p| p.1.as_str()));
        }
        let mut vecs = self.embedder.embed_many(&batch)?.into_iter();
        let mut prepared: std::collections::HashMap<&str, (Vec<f32>, Option<String>, Vec<(&'static str, Vec<f32>)>)> =
            std::collections::HashMap::new();
        let raw: Vec<Vec<f32>> = (0..new_texts.len()).map(|_| vecs.next().unwrap()).collect();
        for ((t, d), rv) in new_texts.iter().zip(&descs).zip(raw) {
            let parts = d.parts.iter().map(|p| (p.0, vecs.next().unwrap())).collect();
            prepared.insert(*t, (rv, d.display.clone(), parts));
        }

        // vectors of already-known texts, copied out BEFORE taking the db lock (lock order is
        // always store -> fixes -> db, never db -> store)
        let known: std::collections::HashMap<&str, (Vec<f32>, Option<String>)> = {
            let st = self.store.read();
            runs.iter()
                .filter_map(|r| st.entry(&r.cmd).filter(|(_, e)| e.has_vec).map(|(i, e)| (r.cmd.as_str(), (st.vec(i).to_vec(), e.desc.clone()))))
                .collect()
        };
        let mut recorded = Vec::with_capacity(runs.len());
        {
            let mut conn = self.db.lock();
            let tx = conn.transaction()?;
            let mut vec_written = std::collections::HashSet::new();
            for r in runs {
                let rec = db::record_run(&tx, r)?;
                if rec.new_row {
                    if let Some((rv, d, parts)) = prepared.get(r.cmd.as_str()) {
                        let first = vec_written.insert(r.cmd.clone());
                        let p: Vec<(&str, Vec<f32>)> = if first { parts.iter().map(|(k, v)| (*k, v.clone())).collect() } else { vec![] };
                        db::write_vectors(&tx, rec.command_id, rv, d.as_deref(), &p)?;
                    } else if let Some((v, d)) = known.get(r.cmd.as_str()) {
                        // new folder for a known command: reuse its vector so the row is complete
                        db::write_vectors(&tx, rec.command_id, v, d.as_deref(), &[])?;
                    }
                }
                recorded.push(rec);
            }
            tx.commit()?;
        }
        {
            let mut st = self.store.write();
            for (r, rec) in runs.iter().zip(&recorded) {
                let v = prepared.get(r.cmd.as_str()).map(|(rv, d, parts)| {
                    let pv: Vec<Vec<f32>> = parts.iter().map(|p| p.1.clone()).collect();
                    (rv.clone(), d.clone(), pv)
                });
                st.apply_run(rec.command_id, rec.new_row, r, v.as_ref().map(|(a, d, p)| (a.as_slice(), d.clone(), p.as_slice())));
            }
        }
        // a command of yours that ran a while has just finished: a desktop notification
        if let Some(after) = self.privacy.lock().done_after {
            let now = config::now();
            for r in runs.iter().filter(|r| r.actor == "human" && r.duration_ms.is_some_and(|d| d >= after as i64 * 1000) && (now - r.ts).abs() <= 120) {
                self.done_alert(r);
            }
        }
        // live fix-pairs
        let mut proven = Vec::new();
        {
            let mut fx = self.fixes.lock();
            for r in runs {
                for p in fx.observe(&fixpairs::stream_key(&r.session, r.cwd.as_deref()), &r.cmd, r.exit) {
                    proven.push((p, r.cwd.clone(), r.ts));
                }
            }
            if !proven.is_empty() {
                let conn = self.db.lock();
                for (p, cwd, ts) in &proven {
                    fx.persist(&conn, p, cwd.as_deref(), *ts)?;
                }
            }
        }
        // flows being walked through: a successful next step advances; a finished flow is dropped
        {
            let mut armed = self.flows_armed.lock();
            if !armed.is_empty() {
                for r in runs {
                    if let Some(f) = armed.get_mut(&r.session) {
                        f.observe(&r.cmd, !r.failed(), r.ts);
                    }
                }
                armed.retain(|_, f| f.pos < f.steps.len());
            }
        }
        let suggestions = runs.iter().map(|r| if r.failed() { self.suggest_for(&r.cmd, r.cwd.as_deref(), r.err.as_deref()) } else { None }).collect();
        Ok((new_texts.len(), suggestions))
    }

    /// The notification for a long command that finished: how it went, how long it took, and
    /// whether that's faster or slower than usual (by a quarter or more).
    fn done_alert(&self, r: &Run) {
        let ms = r.duration_ms.unwrap_or(0).clamp(0, u32::MAX as i64) as u32;
        let took = insight::took(ms, false);
        let usual = {
            let st = self.store.read();
            st.entry(&r.cmd).and_then(|(_, e)| {
                let here = r.cwd.as_deref().map(|c| st.scope_folder(c)).filter(|sc| *sc != Scope::Nothing).unwrap_or(Scope::All);
                st.typical(e, here, true)
            })
        };
        let pace = match usual.filter(|_| r.ok()) {
            Some(u) if (ms as f64) < u as f64 * 0.75 => format!(", faster than usual ({})", insight::took(u, false)),
            Some(u) if (ms as f64) > u as f64 * 1.25 => format!(", slower than usual ({})", insight::took(u, false)),
            _ => String::new(),
        };
        let place = r
            .cwd
            .as_deref()
            .map(config::native_path)
            .map(|c| c.trim_end_matches(['\\', '/']).rsplit(['\\', '/']).next().unwrap_or("").to_string())
            .filter(|p| !p.is_empty())
            .map(|p| format!(" · in {p}"))
            .unwrap_or_default();
        let cmd = insight::short(&r.cmd, 60);
        let (title, body) = if r.ok() {
            (format!("✓ {cmd}"), format!("{took}{pace}{place}"))
        } else if r.failed() {
            (format!("✗ {cmd}"), format!("failed after {took}{place}"))
        } else {
            (cmd, format!("finished after {took}{place}"))
        };
        crate::notify::send(&title, &body);
    }

    /// "Last time here": the first prompt of a session in a folder you last worked in a while
    /// ago gets one line of what you did there. Once per session and folder.
    pub fn welcome(&self, cwd: &str, session: &str) -> Option<String> {
        const AWAY: i64 = 8 * 3600;
        let st = self.store.read();
        let c = st.cwd_index(cwd)?;
        if !self.greeted.lock().insert((session.to_string(), c)) {
            return None;
        }
        let v = insight::last_visit(&st, c, 4)?;
        let now = config::now();
        if now - v.last < AWAY {
            return None;
        }
        let steps: Vec<String> = v.steps.iter().map(|&i| insight::short(&st.entries[i as usize].text, 40)).collect();
        Some(format!("last time here ({}): {}", insight::ago(v.last, now), steps.join(" → ")))
    }

    /// An agent running a command that keeps failing the same way: what to tell it, from the third
    /// identical failure in a row on (and what fixed that error before, when something did).
    pub fn retry_loop_note(&self, cmd: &str, of: db::StreakOf) -> Option<String> {
        const LOOP: u32 = 3;
        let (n, err) = db::same_failures(&self.db.lock(), cmd, of).ok()?;
        if n < LOOP {
            return None;
        }
        let how = err.as_deref().map(|e| format!(" ({})", insight::short(&crate::redact::redact(e), 100))).unwrap_or_default();
        let mut s = format!("`{}` has now failed {n} times in a row, the same way each time{how}. Running it again unchanged will fail again: change something first, or ask the user.", insight::short(cmd, 60));
        let fix = self.suggest_for(cmd, None, err.as_deref()).filter(|v| matches!(v["kind"].as_str(), Some("proven" | "same_error")));
        if let Some(f) = fix.as_ref().and_then(|v| v["command"].as_str()).filter(|f| *f != cmd) {
            s.push_str(&format!(" What fixed this before on this machine: `{}`.", crate::redact::redact(f)));
        }
        Some(s)
    }

    /// A failed command that is flaky here: say so, so nobody changes code over it.
    pub fn flaky_note(&self, cmd: &str, cwd: &str) -> Option<String> {
        let st = self.store.read();
        let (c, (i, _)) = (st.cwd_index(cwd)?, st.entry(cmd)?);
        let f = insight::flaky(&st, i, c)?;
        Some(format!("`{}` is flaky here: {}. Try it again before changing anything.", insight::short(cmd, 40), f.summary()))
    }

    /// "What broke it?": a command that kept working in this folder has just failed. One line:
    /// how often it worked, when last, and what ran here since.
    pub fn broke_note(&self, cmd: &str, cwd: &str) -> Option<String> {
        let st = self.store.read();
        let (c, (i, _)) = (st.cwd_index(cwd)?, st.entry(cmd)?);
        let b = insight::what_broke(&st, i, c)?;
        let since: Vec<String> = b.between.iter().take(3).map(|&j| insight::short(&st.entries[j as usize].text, 32)).collect();
        let more = b.between.len().saturating_sub(3);
        let tail = if since.is_empty() {
            "nothing else ran here in between".to_string()
        } else {
            format!("since then here: {}{}", since.join(" → "), if more > 0 { format!(" (+{more} more)") } else { String::new() })
        };
        let git = insight::GitChange::of(&st, &b).map(|g| format!("; {}", g.clause())).unwrap_or_default();
        Some(format!("`{}` worked here {} times, last {}{git}; {}   (reman why)", insight::short(cmd, 40), b.worked, insight::ago(b.last_ok, config::now()), tail))
    }

    /// A command other than `failed` that failed with this error signature and has a proven fix:
    /// (that command, its fix). The most recently fixed first.
    pub fn fix_for_error(&self, st: &Store, sig: &str, failed: &str) -> Option<(String, String)> {
        let fixes = self.fixes.lock();
        let mut best: Option<(i64, String, String)> = None;
        for &ei in st.by_sig.get(sig)? {
            let text = &st.entries[ei as usize].text;
            if text == failed {
                continue;
            }
            if let Some(p) = fixes.lookup(text, st).into_iter().next() {
                let when = st.entries[ei as usize].rows.iter().map(|r| r.last_used).max().unwrap_or(0);
                if best.as_ref().is_none_or(|b| when > b.0) {
                    best = Some((when, text.clone(), p.fixed));
                }
            }
        }
        best.map(|b| (b.1, b.2))
    }

    /// `reman why [command]`: the story of a command that stopped working in this folder.
    /// Without a command, the last one that failed here.
    fn why_op(&self, req: &Value) -> Value {
        let st = self.store.read();
        let Some(c) = s(req, "cwd").and_then(|p| st.cwd_index(p)) else {
            return json!({"found": false, "reason": "reman has no history for this folder yet."});
        };
        let target = match s(req, "command") {
            Some(t) => st.entry(t.trim()).map(|e| e.0),
            None => st.execs.iter().rev().find(|x| x.cwd == Some(c) && x.exit.is_some_and(|e| e != 0)).map(|x| x.entry),
        };
        let Some(i) = target else {
            return json!({"found": false, "reason": "Nothing has failed in this folder."});
        };
        let e = &st.entries[i as usize];
        let now = config::now();
        if let Some(f) = insight::flaky(&st, i, c) {
            return json!({"found": false, "command": e.text, "flaky": true,
                          "reason": format!("Nothing broke: it's flaky here ({}). Try it again before changing anything.", f.summary())});
        }
        let Some(b) = insight::what_broke(&st, i, c) else {
            let a = st.agg(e, Scope::Folder(c));
            let reason = match (a.ok, a.fail) {
                (0, 0) => "It has no recorded outcome in this folder.".to_string(),
                (0, f) => format!("It has never worked in this folder ({f} failure{}): nothing broke, it never ran right here. `reman fixes` shows what worked instead.", if f == 1 { "" } else { "s" }),
                (_, 0) => "It has not failed in this folder.".to_string(),
                (o, f) => format!("It worked {o} times and failed {f} times here, with no clear point where it broke (it needs 3+ successes, then 1-3 failures in a row)."),
            };
            return json!({"found": false, "command": e.text, "reason": reason});
        };
        let timeline: Vec<Value> = st
            .execs
            .iter()
            .filter(|x| x.cwd == Some(c) && x.ts >= b.last_ok)
            .filter(|x| x.entry == i || !insight::trivial(&st.entries[x.entry as usize]))
            .take(40)
            .map(|x| json!({"command": st.entries[x.entry as usize].text, "exit": x.exit, "ago": insight::ago(x.ts, now), "target": x.entry == i,
                             "branch": st.git_name(x.branch)}))
            .collect();
        let git_at = |(br, hd): (u32, u32)| json!({"branch": st.git_name(br), "commit": st.git_name(hd).map(|c| c.chars().take(7).collect::<String>())});
        let git = insight::GitChange::of(&st, &b).map(|g| json!({"change": g.clause(), "advice": g.advice(), "worked_on": git_at(b.ok_git), "fails_on": git_at(b.fail_git)}));
        json!({"found": true, "command": e.text, "worked": b.worked, "last_ok": insight::ago(b.last_ok, now), "first_fail": insight::ago(b.first_fail, now),
               "between": b.between.iter().map(|&j| st.entries[j as usize].text.clone()).collect::<Vec<_>>(), "timeline": timeline, "git": git})
    }

    /// Enter, before a command runs: does the history here say it will fail? An exact lookup
    /// (this text, this folder) - it runs on every Enter. {"warn": line, "fix": command?} or {}.
    fn precheck_op(&self, req: &Value) -> Value {
        let (Some(cmd), Some(cwd)) = (s(req, "command").map(str::trim), s(req, "cwd")) else { return json!({}) };
        let (w, err, took) = {
            let st = self.store.read();
            let (Some(c), Some((i, e))) = (st.cwd_index(cwd), st.entry(cmd)) else { return json!({}) };
            // only when failing costs something: a command that fails in a blink runs
            let Some(w) = insight::warning(&st, i, c).filter(|_| insight::costly(&st, i, c)) else { return json!({}) };
            (w, st.last_err.get(&i).cloned(), st.typical(e, Scope::Folder(c), false).filter(|ms| *ms >= 10_000))
        };
        let after = took.map(|ms| format!(", after about {} each time", insight::took(ms, true))).unwrap_or_default();
        let warn = if w.worked == 0 {
            format!("this failed all {} times it ran here{after}", w.failed)
        } else {
            format!("this failed the last {} times here{after} (it worked {} time{} before that)", w.failed, w.worked, if w.worked == 1 { "" } else { "s" })
        };
        let sg = self.suggest_for(cmd, Some(cwd), err.as_deref()).filter(|v| v["command"].as_str().is_some_and(|f| f != cmd));
        let (fix, diff) = sg.map_or((None, None), |v| (v["command"].as_str().map(str::to_string), v["diff"].as_str().map(str::to_string)));
        let error = err.as_deref().map(|e| insight::short(e, 72)).filter(|e| !e.is_empty());
        json!({"warn": warn, "fix": fix, "error": error, "diff": diff})
    }

    /// `reman agents`: what each agent (and you) ran over the last `days`: runs, failures, time
    /// lost to failures, and the commands an agent kept retrying the same way, failing each time.
    fn agents_op(&self, req: &Value) -> Result<Value> {
        const LOOP: u32 = 4;
        let days = n(req, "days", 7).clamp(1, 365);
        let since = config::now() - days * 86400;
        let rows: Vec<(String, String, String, Option<i64>, Option<String>, i64, Option<i64>, Option<String>)> = {
            let conn = self.db.lock();
            let mut st = conn.prepare(
                "SELECT e.actor, COALESCE(e.session, ''), c.cmd_text, e.exit, e.err, e.ts, e.duration_ms, e.cwd
                 FROM executions e JOIN commands c ON c.id = e.command_id WHERE e.ts >= ?1 ORDER BY e.ts, e.id",
            )?;
            st.query_map([since], |r| Ok((r.get::<_, Option<String>>(0)?.unwrap_or_default(), r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?)))?
                .collect::<rusqlite::Result<_>>()?
        };
        #[derive(Default)]
        struct Tally {
            runs: u64,
            failed: u64,
            unknown: u64,
            lost_ms: i64,
        }
        let mut by: std::collections::BTreeMap<String, Tally> = std::collections::BTreeMap::new();
        // (actor, session, command) -> (the way the current streak fails, its length, last ts, folder)
        let mut streak: std::collections::HashMap<(String, String, String), (String, u32, i64, Option<String>)> = std::collections::HashMap::new();
        let mut loops: Vec<(u32, i64, String, String, Option<String>)> = Vec::new();
        let close = |k: &(String, String, String), s: &(String, u32, i64, Option<String>), loops: &mut Vec<_>| {
            if s.1 >= LOOP {
                loops.push((s.1, s.2, k.2.clone(), k.0.clone(), s.3.clone()));
            }
        };
        for (actor, session, cmd, exit, err, ts, dur, cwd) in rows {
            let t = by.entry(actor.clone()).or_default();
            t.runs += 1;
            match exit {
                Some(e) if e > 0 => {
                    t.failed += 1;
                    t.lost_ms += dur.unwrap_or(0).max(0);
                }
                Some(0) => {}
                _ => t.unknown += 1,
            }
            if !actor.starts_with("agent:") || session.is_empty() {
                continue;
            }
            let key = (actor, session, cmd);
            let failed = exit.is_some_and(|e| e > 0);
            let way = err.as_deref().and_then(crate::errors::signature).unwrap_or_else(|| format!("exit {}", exit.unwrap_or(0)));
            match streak.get_mut(&key) {
                Some(s) if failed && s.0 == way => {
                    s.1 += 1;
                    s.2 = ts;
                    s.3 = cwd;
                }
                _ => {
                    if let Some(s) = streak.remove(&key) {
                        close(&key, &s, &mut loops);
                    }
                    if failed {
                        streak.insert(key, (way, 1, ts, cwd));
                    }
                }
            }
        }
        for (k, s) in &streak {
            close(k, s, &mut loops);
        }
        loops.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
        let now = config::now();
        let st = self.store.read();
        let who = |a: &str| if a == "human" { "you".to_string() } else { a.strip_prefix("agent:").unwrap_or(a).to_string() };
        let mut actors: Vec<(&String, &Tally)> = by.iter().collect();
        actors.sort_by(|a, b| (a.0 == "human").cmp(&(b.0 == "human")).then(b.1.runs.cmp(&a.1.runs)));
        Ok(json!({
            "days": days,
            "actors": actors.iter().map(|(a, t)| json!({"actor": who(a), "runs": t.runs, "failed": t.failed, "unknown": t.unknown, "lost_ms": t.lost_ms})).collect::<Vec<_>>(),
            "loops": loops.iter().take(10).map(|(n, ts, cmd, a, cwd)| json!({"command": cmd, "actor": who(a), "times": n, "ago": insight::ago(*ts, now),
                "folder": cwd.as_deref().and_then(|c| st.cwd_index(c)).map(|c| st.cwd_name(c).to_string())})).collect::<Vec<_>>(),
        }))
    }

    /// `reman yesterday` / `today` / `day <date>`: what ran that day, by project (repo, else
    /// folder) in time order: your commands with repeats folded and looking-around left out,
    /// failures with what fixed them (or that they're still failing), and the agents in one line.
    fn day_op(&self, req: &Value) -> Result<Value> {
        const FLOW_MAX: usize = 8;
        let offset = self.local_offset();
        let name = s(req, "day").unwrap_or("today");
        let Some((since, until)) = crate::timewords::day(name, config::now(), offset) else {
            return Ok(json!({"found": false, "reason": format!("`{name}` isn't a day reman knows (today, yesterday, a weekday, or YYYY-MM-DD).")}));
        };
        let rows: Vec<(String, String, Option<i64>, i64, Option<String>, Option<String>)> = {
            let conn = self.db.lock();
            let mut st = conn.prepare(
                "SELECT COALESCE(e.actor, ''), c.cmd_text, e.exit, e.ts, e.cwd, e.branch FROM executions e JOIN commands c ON c.id = e.command_id
                 WHERE e.ts >= ?1 AND e.ts < ?2 ORDER BY e.ts, e.id",
            )?;
            st.query_map([since, until], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?.collect::<rusqlite::Result<_>>()?
        };
        let hhmm = |ts: i64| {
            let m = (ts + offset).rem_euclid(86_400) / 60;
            format!("{}:{:02}", m / 60, m % 60)
        };
        #[derive(Default)]
        struct Proj {
            name: String,
            from: i64,
            to: i64,
            branches: Vec<String>,
            flow: Vec<(String, u32)>,
            /// command -> (failures, worked later that day)
            failed: Vec<(String, u32, bool)>,
            oks: Vec<(String, i64)>,
            agents: std::collections::BTreeMap<String, (u32, u32)>,
        }
        let st = self.store.read();
        let mut order: Vec<String> = Vec::new();
        let mut projects: std::collections::HashMap<String, Proj> = std::collections::HashMap::new();
        for (actor, cmd, exit, ts, cwd, branch) in &rows {
            let key = cwd.as_deref().map(crate::git::repo_identity).unwrap_or_else(|| "(no folder)".into());
            let p = projects.entry(key.clone()).or_insert_with(|| {
                order.push(key.clone());
                let name = key.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().unwrap_or(&key).to_string();
                Proj { name, from: *ts, ..Default::default() }
            });
            p.to = *ts;
            if let Some(b) = branch.as_ref().filter(|b| !p.branches.contains(b)) {
                p.branches.push(b.clone());
            }
            let failed = exit.is_some_and(|e| e > 0);
            if let Some(a) = actor.strip_prefix("agent:") {
                let t = p.agents.entry(a.to_string()).or_default();
                t.0 += 1;
                t.1 += failed as u32;
                continue;
            }
            if st.entry(cmd).is_some_and(|(_, e)| insight::trivial(e)) {
                continue;
            }
            if failed {
                match p.failed.iter_mut().find(|f| f.0 == *cmd) {
                    Some(f) => f.1 += 1,
                    None => p.failed.push((cmd.clone(), 1, false)),
                }
                continue;
            }
            if *exit == Some(0) {
                p.oks.push((cmd.clone(), *ts));
                if let Some(f) = p.failed.iter_mut().find(|f| f.0 == *cmd) {
                    f.2 = true;
                }
            }
            match p.flow.last_mut() {
                Some(l) if l.0 == *cmd => l.1 += 1,
                _ => match p.flow.iter_mut().find(|l| l.0 == *cmd) {
                    Some(l) => l.1 += 1,
                    None => p.flow.push((cmd.clone(), 1)),
                },
            }
        }
        let fixes = self.fixes.lock();
        let out: Vec<Value> = order
            .iter()
            .map(|k| {
                let p = &projects[k];
                let failures: Vec<Value> = p
                    .failed
                    .iter()
                    .map(|(cmd, n, worked)| {
                        // what fixed it: a proven fix for it that ran (and worked) here that day
                        let fix = (!worked).then(|| fixes.lookup(cmd, &st).into_iter().find(|f| p.oks.iter().any(|o| o.0 == f.fixed))).flatten().map(|f| f.fixed);
                        json!({"command": cmd, "times": n, "worked_later": worked, "fixed_by": fix})
                    })
                    .collect();
                let more = p.flow.len().saturating_sub(FLOW_MAX);
                json!({"name": p.name, "branch": p.branches.last(), "from": hhmm(p.from), "to": hhmm(p.to),
                       "flow": p.flow.iter().take(FLOW_MAX).map(|(c, n)| json!({"command": c, "times": n})).collect::<Vec<_>>(), "more": more,
                       "failures": failures,
                       "agents": p.agents.iter().map(|(a, (r, f))| json!({"agent": a, "runs": r, "failed": f})).collect::<Vec<_>>()})
            })
            .collect();
        let span = rows.first().zip(rows.last()).map(|(a, b)| (hhmm(a.3), hhmm(b.3)));
        Ok(json!({"found": !rows.is_empty(), "day": name, "from": span.as_ref().map(|s| s.0.clone()), "to": span.map(|s| s.1), "projects": out}))
    }

    /// `reman goto <words>` (`rcd` in the shells): the folders where you ran what the words
    /// describe, or whose path has them, best first, that still exist. With no words, the folders
    /// you were in most recently. {"results": [{"folder", "because", "ago"}]}
    fn goto_op(&self, req: &Value) -> Result<Value> {
        let words = s(req, "query").unwrap_or("").trim().to_string();
        let k = n(req, "k", if words.is_empty() { 10 } else { 3 }).clamp(1, 20) as usize;
        let qv = if words.chars().count() >= 3 { Some(self.embedder.embed_query(&words)?) } else { None };
        let st = self.store.read();
        let now = config::now();
        // as this OS writes it (a folder recorded from Git Bash is `/c/...`)
        let exists = |c: u32| std::path::Path::new(&config::native_path(st.cwd_name(c))).is_dir();
        // when each folder was last worked in
        let mut last: std::collections::HashMap<u32, i64> = std::collections::HashMap::new();
        for x in &st.execs {
            if let Some(c) = x.cwd {
                last.insert(c, x.ts);
            }
        }
        // folder -> (score, the command that put it here)
        let mut best: std::collections::HashMap<u32, (f32, Option<u32>)> = std::collections::HashMap::new();
        let mut offer = |c: u32, score: f32, because: Option<u32>| {
            let b = best.entry(c).or_insert((f32::MIN, None));
            if score > b.0 {
                *b = (score, because.or(b.1));
            }
        };
        if words.is_empty() {
            for (&c, &ts) in &last {
                offer(c, ts as f32, None);
            }
        } else {
            let q = Query { text: &words, k: 40, offset: 0, scope: Scope::All, actor: None, status: None, group: false, rank: Rank::Hybrid, here: None, window: None };
            for h in search::search(&st, &q, qv.as_deref().map(|v| v.as_slice())).hits.iter().filter(|h| h.close) {
                let e = &st.entries[h.idx as usize];
                if insight::trivial(e) {
                    continue; // `cd x` and `ls` say nothing about where the work is
                }
                for c in e.rows.iter().filter_map(|r| r.cwd) {
                    offer(c, h.score, Some(h.idx));
                }
            }
            // the folder's own name: every word in its last part (`planet api` → planet_naidu_api)
            // outranks any command match; every word somewhere in its path ranks with them
            const NAMED: f32 = 1000.0;
            let squash = |t: &str| t.to_lowercase().replace(['_', '-', ' ', '.'], "");
            let terms: Vec<String> = words.split_whitespace().map(squash).collect();
            for c in 0..st.cwd_count() {
                let path = squash(st.cwd_name(c));
                let tail = squash(st.cwd_name(c).trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().unwrap_or(""));
                if terms.iter().all(|t| tail.contains(t.as_str())) {
                    // the closer the name is to the words, the better (`api` → `api`, not `api-old`)
                    offer(c, NAMED - (tail.len() as f32 - terms.iter().map(String::len).sum::<usize>() as f32).abs(), None);
                } else if terms.iter().all(|t| path.contains(t.as_str())) {
                    offer(c, 0.6, None);
                }
            }
        }
        let mut ranked: Vec<(u32, f32, Option<u32>)> = best.into_iter().map(|(c, (s, b))| (c, s, b)).filter(|(c, ..)| exists(*c)).collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(last.get(&b.0).cmp(&last.get(&a.0))));
        let results: Vec<Value> = ranked
            .iter()
            .take(k)
            .map(|(c, _, because)| {
                json!({"folder": config::native_path(st.cwd_name(*c)), "because": because.map(|i| st.entries[i as usize].text.clone()),
                       "ago": last.get(c).map(|t| insight::ago(*t, now))})
            })
            .collect();
        Ok(json!({"results": results}))
    }

    /// `reman here`: what you did here last time, however long ago.
    fn here_op(&self, req: &Value) -> Value {
        let st = self.store.read();
        let v = s(req, "cwd").and_then(|p| st.cwd_index(p)).and_then(|c| insight::last_visit(&st, c, 10));
        match v {
            None => json!({"found": false}),
            Some(v) => json!({"found": true, "ago": insight::ago(v.last, config::now()),
                              "steps": v.steps.iter().map(|&i| st.entries[i as usize].text.clone()).collect::<Vec<_>>()}),
        }
    }

    /// `reman runbook`: how this project is run (its repo, else this folder).
    fn runbook_op(&self, req: &Value) -> Value {
        let st = self.store.read();
        let Some(cwd) = s(req, "cwd") else { return json!({"found": false}) };
        let folder = st.cwd_index(cwd);
        let scope = match st.scope_repo(cwd) {
            Scope::Nothing => folder.map(Scope::Folder).unwrap_or(Scope::Nothing),
            r => r,
        };
        if scope == Scope::Nothing {
            return json!({"found": false});
        }
        let rb = insight::runbook_json(&st, scope, cwd, &|_, t, _| Some(t.to_string()), true);
        // the model's fuller version, while the project's commands are the same ("fresh": the
        // runbook from the history alone, with what only a model needs, to ask it again)
        if b(req, "fresh", false) {
            return rb;
        }
        match crate::ai::cached(&rb) {
            Some((x, model)) => crate::ai::merge(&rb, &x, &model),
            None => rb,
        }
    }

    /// One-line hint after a failure: a proven fix, else a strong typo match that worked.
    pub fn suggest(&self, failed: &str, cwd: Option<&str>) -> Option<Value> {
        self.suggest_for(failed, cwd, None)
    }

    /// After a failure: a proven fix for this command; else one for a DIFFERENT command that
    /// failed with the same error (`err`, see errors.rs); else a strong typo match that worked.
    pub fn suggest_for(&self, failed: &str, cwd: Option<&str>, err: Option<&str>) -> Option<Value> {
        // what the fix changes, word by word, when it is a variant of the failed command (after
        // "command": shell hooks read that field first)
        let mut v = self.suggestion(failed, cwd, err)?;
        if let Some(d) = v["command"].as_str().and_then(|f| fixpairs::diff(failed, f)) {
            v["diff"] = json!(d);
        }
        Some(v)
    }

    fn suggestion(&self, failed: &str, cwd: Option<&str>, err: Option<&str>) -> Option<Value> {
        let st = self.store.read();
        if let Some(p) = self.fixes.lock().lookup(failed, &st).into_iter().next() {
            return Some(json!({"command": p.fixed, "kind": "proven", "confidence": p.confidence, "times": p.count}));
        }
        if let Some((other, fix)) = err.and_then(crate::errors::signature).and_then(|sig| self.fix_for_error(&st, &sig, failed)) {
            return Some(json!({"command": fix, "kind": "same_error", "failed": other}));
        }
        // prefer a strong typo match from this folder, else a strong one from anywhere
        const STRONG: f32 = 0.8;
        let strong = |scope: Scope| dym::did_you_mean(&st, failed, None, 1, true, scope).into_iter().next().filter(|s| s.typo >= STRONG);
        let folder = cwd.map(|c| st.scope_folder(c)).filter(|s| *s != Scope::Nothing);
        let best = folder.and_then(strong).or_else(|| strong(Scope::All))?;
        Some(json!({"command": st.entries[best.idx as usize].text, "kind": "typo", "confidence": (best.typo * 1000.0).round() / 1000.0}))
    }

    fn scope_of(&self, st: &Store, req: &Value) -> (Scope, Option<u32>) {
        let cwd = s(req, "cwd");
        let here = cwd.and_then(|c| st.cwd_index(c));
        let scope = match (s(req, "scope"), cwd) {
            (Some("all"), _) | (_, None) => Scope::All,
            (Some("repo"), Some(c)) => st.scope_repo(c),
            (_, Some(c)) => st.scope_folder(c),
        };
        (scope, here)
    }

    pub fn item(&self, st: &Store, h: &search::Hit, scope: Scope) -> Value {
        let e = &st.entries[h.idx as usize];
        let a = st.agg(e, scope);
        let a_all = if scope == Scope::All { a.clone() } else { st.agg(e, Scope::All) };
        let mut v = json!({
            "command": e.text,
            "actor": a.actor_label(),
            "status": a.status(),
            "runs": a_all.runs, "run_count": a_all.runs,
            "success_rate": a_all.success_rate(),
            "last_run": config::age(a.last_used), "last_used": a.last_used,
            // how long it usually takes when it works: here, else anywhere
            "typical_ms": st.typical(e, scope, true).or_else(|| st.typical(e, Scope::All, true)),
            "intent": e.gkey, "description": e.desc,
            "cwd": a.cwd.map(|c| st.cwd_name(c)),
            "folders": a_all.folders, "pinned": e.pinned,
        });
        if h.sim > -1.0 {
            v["similarity"] = json!((h.sim * 1000.0).round() / 1000.0);
        }
        v["close"] = json!(h.close);
        if let Some(e) = st.last_err.get(&h.idx) {
            v["last_error"] = json!(e);
        }
        if h.words > 0.0 {
            v["words"] = json!((h.words * 100.0).round() / 100.0);
        }
        if h.fuzzy > 0.0 {
            v["fuzzy"] = json!((h.fuzzy * 1000.0).round() / 1000.0);
        }
        if h.variants > 1 {
            v["variants"] = json!(h.variants);
        }
        if h.matched_terms > 0 {
            v["matched_terms"] = json!(h.matched_terms);
        }
        v
    }

    /// The local time's distance from UTC, in seconds (SQLite knows the time zone).
    pub fn local_offset(&self) -> i64 {
        self.db
            .lock()
            .query_row("SELECT CAST(strftime('%s','now','localtime') AS INTEGER) - CAST(strftime('%s','now') AS INTEGER)", [], |r| r.get(0))
            .unwrap_or(0)
    }

    fn search_op(&self, req: &Value, browse: bool) -> Result<Value> {
        // a time named at the end (`deploy last week`) filters; the rest is what to look for
        let (words, window) = if browse { (String::new(), None) } else { crate::timewords::split(s(req, "query").unwrap_or(""), config::now(), self.local_offset()) };
        let text = words.as_str();
        let rank = if s(req, "rank") == Some("semantic") { Rank::Semantic } else { Rank::Hybrid };
        // embed before taking the store lock; tiny queries go fuzzy-only (no model call at all)
        let t0 = Instant::now();
        let qv = if text.trim().chars().count() >= 3 { Some(self.embedder.embed_query(text.trim())?) } else { None };
        let t_embed = t0.elapsed();
        let st = self.store.read();
        let (scope, here) = self.scope_of(&st, req);
        let k = n(req, "k", if browse { 40 } else { 5 });
        let q = Query {
            text,
            k: if k <= 0 { 0 } else { k as usize },
            offset: n(req, "offset", 0).max(0) as usize,
            scope,
            actor: s(req, "actor"),
            status: s(req, "status"),
            group: b(req, "group", !browse),
            rank,
            here,
            window: window.as_ref().map(|w| (w.since, w.until)),
        };
        let out = search::search(&st, &q, qv.as_deref().map(|v| v.as_slice()));
        let t_rank = t0.elapsed() - t_embed;
        let mut results: Vec<Value> = out.hits.iter().map(|h| self.item(&st, h, scope)).collect();
        // the finder (only it asks): a folded group that differs in one place is a command with a
        // blank, unless what was typed names one of its values
        if b(req, "templates", false) {
            for (v, h) in results.iter_mut().zip(&out.hits) {
                if let Some(m) = out.members.get(&h.idx) {
                    self.add_template(&st, v, m, scope, text);
                }
            }
        }
        if std::env::var_os("REMAN_TRACE").is_some() {
            log(&format!("search {text:?}: embed {t_embed:?} rank {t_rank:?} total {:?}", t0.elapsed()));
        }
        let mut v = json!({"mode": out.mode, "confident": out.confident, "total": out.total, "results": results});
        if let Some(w) = window {
            v["window"] = json!(w.label);
        }
        Ok(v)
    }

    /// Turn a folded item into a command with a blank (describe::template): its display and fill,
    /// the values it took (most recent first), and the track record of all of them together.
    fn add_template(&self, st: &Store, v: &mut Value, members: &[u32], scope: Scope, query: &str) {
        let mut m: Vec<u32> = members.to_vec();
        m.sort_by_key(|&i| std::cmp::Reverse(st.entries[i as usize].rows.iter().map(|r| r.last_used).max().unwrap_or(0)));
        let texts: Vec<&str> = m.iter().map(|&i| st.entries[i as usize].text.as_str()).collect();
        let Some(t) = describe::template(&texts) else { return };
        // `fix login` names the commit message: show that commit, not the blank (words of the
        // command itself, `git commit`, name nothing)
        let fixed = t.display.to_lowercase();
        let words: Vec<String> = query.to_lowercase().split_whitespace().filter(|w| w.chars().count() >= 3 && !fixed.contains(*w)).map(String::from).collect();
        if t.values.iter().any(|val| words.iter().any(|w| val.to_lowercase().contains(w.as_str()))) {
            return;
        }
        let (mut runs, mut ok, mut fail, mut last) = (0u32, 0u32, 0u32, 0i64);
        for &i in &m {
            let a = st.agg(&st.entries[i as usize], scope);
            runs += a.runs;
            ok += a.ok;
            fail += a.fail;
            last = last.max(a.last_used);
        }
        v["template"] = json!(t.display);
        v["fill"] = json!(t.fill);
        v["blank"] = json!(t.kind);
        v["values"] = json!(t.values);
        v["runs"] = json!(runs);
        v["status"] = json!(crate::store::status_of(ok, fail));
        v["success_rate"] = json!(((ok + fail) > 0).then(|| ((ok as f64) / ((ok + fail) as f64) * 100.0).round() / 100.0));
        v["last_used"] = json!(last);
    }

    fn dym_op(&self, req: &Value) -> Result<Value> {
        let q = s(req, "query").unwrap_or("");
        let qv = self.embedder.embed_query(q)?;
        let st = self.store.read();
        let scope = s(req, "here").map(|c| st.scope_folder(c)).unwrap_or(Scope::All);
        let k = n(req, "k", 8).max(1) as usize;
        let mut results: Vec<Value> = Vec::new();
        for p in self.fixes.lock().lookup(q, &st).into_iter().take(k) {
            let Some((i, _)) = st.entry(&p.fixed) else { continue };
            let mut v = self.item(&st, &search::Hit { idx: i, score: 1.0, sim: -2.0, fuzzy: 0.0, variants: 1, matched_terms: 0, close: true, words: 0.0 }, Scope::All);
            v["proven"] = json!(true);
            v["fix_confidence"] = json!(p.confidence);
            v["similarity"] = json!(1.0);
            results.push(v);
        }
        for sg in dym::did_you_mean(&st, q, Some(&qv), k, b(req, "worked_only", false), scope) {
            let text = &st.entries[sg.idx as usize].text;
            if results.len() >= k || results.iter().any(|r| r["command"] == json!(text)) {
                continue;
            }
            let mut v = self.item(&st, &search::Hit { idx: sg.idx, score: sg.score, sim: -2.0, fuzzy: 0.0, variants: 1, matched_terms: 0, close: true, words: 0.0 }, Scope::All);
            v["similarity"] = json!((sg.score * 1000.0).round() / 1000.0);
            v["typo"] = json!((sg.typo * 1000.0).round() / 1000.0);
            v["intent_sim"] = json!((sg.sem * 1000.0).round() / 1000.0);
            results.push(v);
        }
        Ok(json!({"results": results}))
    }

    /// The flow this session is walking through: remaining steps as full items, next one first.
    fn flow_progress(&self, session: &str) -> Option<Value> {
        let f = {
            let mut armed = self.flows_armed.lock();
            let now = config::now();
            armed.retain(|_, f| now - f.touched <= FLOW_IDLE_S && f.pos < f.steps.len());
            armed.get(session).cloned()?
        };
        let st = self.store.read();
        let items: Vec<Value> = f.steps[f.pos..]
            .iter()
            .enumerate()
            .map(|(i, cmd)| {
                let mut v = match st.entry(cmd) {
                    Some((idx, _)) => self.item(&st, &search::Hit { idx, score: 0.0, sim: -2.0, fuzzy: 0.0, variants: 1, matched_terms: 0, close: true, words: 0.0 }, Scope::All),
                    None => json!({"command": cmd, "status": "unknown", "actor": "human"}),
                };
                v["flow_step"] = json!(f.pos + i + 1);
                v["flow_total"] = json!(f.steps.len());
                v
            })
            .collect();
        Some(json!({"steps": f.steps, "pos": f.pos, "items": items}))
    }

    fn folders_op(&self, req: &Value) -> Value {
        let st = self.store.read();
        let mut runs: std::collections::HashMap<u32, (u32, i64)> = std::collections::HashMap::new();
        for e in st.entries.iter().filter(|e| e.alive) {
            for r in e.rows.iter().filter(|r| !st.row_unscoped(r)) {
                if let Some(c) = r.cwd {
                    let x = runs.entry(c).or_default();
                    x.0 += r.runs;
                    x.1 = x.1.max(r.last_used);
                }
            }
        }
        let mut v: Vec<(u32, (u32, i64))> = runs.into_iter().collect();
        v.sort_by(|a, b| b.1.0.cmp(&a.1.0));
        let k = n(req, "k", 50).max(1) as usize;
        json!({"results": v.iter().take(k).map(|(c, (n, last))| json!({"cwd": st.cwd_name(*c), "runs": n, "last_used": last})).collect::<Vec<_>>()})
    }

    fn next_op(&self, req: &Value) -> Value {
        let mut out = self.predict_op(req);
        if let Some(fp) = s(req, "session").and_then(|x| self.flow_progress(x)) {
            out["flow"] = fp;
        }
        // the finder opened right after a command failed in this shell: lead with its fix
        let failed = {
            let st = self.store.read();
            s(req, "session")
                .and_then(|x| st.session_index(x))
                .and_then(|si| st.execs.iter().rev().find(|x| x.session == si))
                .filter(|x| x.exit.is_some_and(|e| e != 0) && config::now() - x.ts <= 600)
                .map(|x| st.entries[x.entry as usize].text.clone())
        };
        if let Some(f) = failed {
            if let Some(sg) = self.suggest(&f, s(req, "cwd")) {
                let st = self.store.read();
                if let Some((i, _)) = st.entry(sg["command"].as_str().unwrap_or("")) {
                    let mut v = self.item(&st, &search::Hit { idx: i, score: 1.0, sim: -2.0, fuzzy: 0.0, variants: 1, matched_terms: 0, close: true, words: 0.0 }, Scope::All);
                    v["proven"] = json!(sg["kind"] == "proven");
                    v["times_fixed"] = sg["times"].clone();
                    out["fix"] = json!({"failed": f, "item": v});
                }
            }
        }
        out
    }

    /// Alt+N on an empty prompt: what you'd run next here, one idea at a time (`index` cycles
    /// through up to 3): the next step of a flow you're walking through, what worked last time
    /// the command you just ran failed, then what usually follows here.
    /// {"command", "reason", "index", "of"}, or {} with nothing to offer.
    fn nextup_op(&self, req: &Value) -> Value {
        const MAX: usize = 3;
        let mut q = req.clone();
        q["k"] = json!(12);
        let nx = self.next_op(&q);
        let mut ideas: Vec<(String, String)> = Vec::new();
        // for your prompt: commands you ran yourself (an agent's long one-liners aren't), on one line
        let yours = {
            let st = self.store.read();
            move |c: &str| c.chars().count() <= 160 && !c.contains('\n') && st.entry(c).is_some_and(|(_, e)| e.rows.iter().any(|r| r.human > 0))
        };
        let add = |ideas: &mut Vec<(String, String)>, c: Option<&str>, why: String| {
            if let Some(c) = c.filter(|c| !c.trim().is_empty() && yours(c) && !ideas.iter().any(|i| i.0 == *c)) {
                ideas.push((c.to_string(), why));
            }
        };
        if let Some(i) = nx["flow"]["items"].get(0) {
            add(&mut ideas, i["command"].as_str(), format!("the next step of the flow you started ({} of {})", i["flow_step"], i["flow_total"]));
        }
        if nx.get("fix").is_some() {
            let failed = insight::short(nx["fix"]["failed"].as_str().unwrap_or(""), 40);
            add(&mut ideas, nx["fix"]["item"]["command"].as_str(), format!("worked last time `{failed}` failed"));
        }
        for p in nx["results"].as_array().into_iter().flatten() {
            add(&mut ideas, p["command"].as_str(), p["reason"].as_str().unwrap_or("usual here").to_string());
        }
        ideas.truncate(MAX);
        if ideas.is_empty() {
            return json!({});
        }
        let i = (n(req, "index", 0).max(0) as usize) % ideas.len();
        json!({"command": ideas[i].0, "reason": ideas[i].1, "index": i, "of": ideas.len()})
    }

    fn predict_op(&self, req: &Value) -> Value {
        let st = self.store.read();
        let cwd = s(req, "cwd").and_then(|c| st.cwd_index(c));
        let (last, prev);
        if let Some(lc) = s(req, "last_command") {
            last = st.entry(lc).map(|e| e.0);
            prev = None;
        } else {
            let sess = s(req, "session").and_then(|x| st.session_index(x));
            (last, prev) = predict::context(&st, cwd, sess);
        }
        let k = n(req, "k", 5).max(1) as usize;
        let preds = predict::predict(&st, cwd, last, prev, k);
        let results: Vec<Value> = preds
            .iter()
            .map(|p| {
                let mut v = self.item(&st, &search::Hit { idx: p.idx, score: p.score, sim: -2.0, fuzzy: 0.0, variants: 1, matched_terms: 0, close: true, words: 0.0 }, Scope::All);
                v["reason"] = json!(p.reason);
                v["predicted"] = json!(true);
                v
            })
            .collect();
        json!({"last": last.map(|l| st.entries[l as usize].text.clone()), "results": results})
    }

    fn flows_op(&self, req: &Value) -> Value {
        let st = self.store.read();
        let cwd = match s(req, "cwd") {
            Some(c) => match st.cwd_index(c) {
                Some(i) => Some(i),
                None => return json!({"results": []}),
            },
            None => None,
        };
        let fl = flows::detect(&st, cwd, n(req, "min_count", 2).max(1) as u32, 4, 300, n(req, "k", 40).max(1) as usize);
        let results: Vec<Value> = fl
            .iter()
            .map(|f| {
                json!({"sequence": f.seq.iter().map(|&i| st.entries[i as usize].text.clone()).collect::<Vec<_>>(),
                       "count": f.count, "length": f.seq.len()})
            })
            .collect();
        json!({"results": results})
    }

    fn forget(&self, text: &str) -> Result<usize> {
        let ids = self.store.read().row_ids(text);
        if ids.is_empty() {
            return Ok(0);
        }
        {
            let conn = self.db.lock();
            db::delete_command_rows(&conn, &ids)?;
            conn.execute("DELETE FROM fix_pairs WHERE failed_text=? OR fixed_cmd=?", [text, text])?;
        }
        self.store.write().remove_rows(&ids);
        Ok(ids.len())
    }

    /// Retention: drop (command, folder) rows that only ever failed and are older than `days`.
    pub fn purge_failed(&self, days: i64) -> Result<usize> {
        let cutoff = config::now() - days * 86400;
        let ids: Vec<i64> = {
            let st = self.store.read();
            st.entries
                .iter()
                .flat_map(|e| e.rows.iter())
                .filter(|r| r.fail > 0 && r.ok == 0 && r.last_used < cutoff && !r.id.is_negative())
                .map(|r| r.id)
                .collect()
        };
        if ids.is_empty() {
            return Ok(0);
        }
        {
            let mut conn = self.db.lock();
            let tx = conn.transaction()?;
            db::delete_command_rows(&tx, &ids)?;
            tx.commit()?;
        }
        self.store.write().remove_rows(&ids);
        Ok(ids.len())
    }

    /// Re-embed every command (+ its tldr descriptions) with the current model, then reload.
    fn reindex(&self) -> Result<usize> {
        let work: Vec<(String, Vec<i64>)> = {
            let st = self.store.read();
            st.entries.iter().filter(|e| e.alive).map(|e| (e.text.clone(), e.rows.iter().map(|r| r.id).collect())).collect()
        };
        self.embed_rows(&work)?;
        self.reload()?;
        Ok(work.len())
    }

    /// Embed each (command, its row ids) and its descriptions again, and save them.
    fn embed_rows(&self, work: &[(String, Vec<i64>)]) -> Result<()> {
        for chunk in work.chunks(256) {
            let descs: Vec<describe::Description> = chunk.iter().map(|(t, _)| describe::describe(t)).collect();
            let mut batch: Vec<&str> = chunk.iter().map(|(t, _)| t.as_str()).collect();
            for d in &descs {
                batch.extend(d.parts.iter().map(|p| p.1.as_str()));
            }
            let mut vecs = self.embedder.embed_many(&batch)?.into_iter();
            let raw: Vec<Vec<f32>> = (0..chunk.len()).map(|_| vecs.next().unwrap()).collect();
            let mut conn = self.db.lock();
            let tx = conn.transaction()?;
            for (((_, ids), d), rv) in chunk.iter().zip(&descs).zip(raw) {
                let parts: Vec<(&str, Vec<f32>)> = d.parts.iter().map(|p| (p.0, vecs.next().unwrap())).collect();
                for (i, id) in ids.iter().enumerate() {
                    db::write_vectors(&tx, *id, &rv, d.display.as_deref(), if i == 0 { &parts } else { &[] })?;
                }
            }
            tx.commit()?;
        }
        Ok(())
    }

    /// `reman scrub`: history saved before secrets were masked at capture. A dry run by default
    /// (how many would be masked or dropped, with examples); `apply` does it. A masked command
    /// keeps its runs (merged into the masked one when that exists); one whose secret can't be
    /// located is forgotten; learned fixes mentioning either go.
    fn scrub(&self, apply: bool) -> Result<Value> {
        let (mask, drop): (Vec<(String, String)>, Vec<String>) = {
            let st = self.store.read();
            let (mut m, mut d) = (Vec::new(), Vec::new());
            for e in st.entries.iter().filter(|e| e.alive) {
                let masked = crate::redact::redact(&e.text);
                if crate::redact::residual_secret(&masked) {
                    d.push(e.text.clone());
                } else if masked != e.text {
                    m.push((e.text.clone(), masked));
                }
            }
            (m, d)
        };
        let examples: Vec<String> = mask.iter().take(5).map(|(_, m)| insight::short(m, 80)).collect();
        let mut out = json!({"mask": mask.len(), "drop": drop.len(), "examples": examples, "applied": false});
        if !apply || (mask.is_empty() && drop.is_empty()) {
            return Ok(out);
        }
        for t in &drop {
            self.forget(t)?;
        }
        let mut masked_texts: Vec<String> = Vec::new();
        {
            let mut conn = self.db.lock();
            let tx = conn.transaction()?;
            for (orig, masked) in &mask {
                let rows: Vec<(i64, Option<String>)> = {
                    let mut q = tx.prepare("SELECT id, cwd FROM commands WHERE cmd_text=?")?;
                    let r = q.query_map([orig], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?;
                    r
                };
                for (id, cwd) in rows {
                    let hash = db::cmd_hash(masked, cwd.as_deref());
                    let other: Option<i64> = tx.query_row("SELECT id FROM commands WHERE cmd_hash=?", [&hash], |r| r.get(0)).ok();
                    match other {
                        Some(x) if x != id => {
                            // the masked command already has this folder's row: add the runs to it
                            tx.execute(
                                "UPDATE commands SET
                                   run_count = COALESCE(run_count,0) + (SELECT COALESCE(run_count,0) FROM commands WHERE id=?2),
                                   success_count = COALESCE(success_count,0) + (SELECT COALESCE(success_count,0) FROM commands WHERE id=?2),
                                   fail_count = COALESCE(fail_count,0) + (SELECT COALESCE(fail_count,0) FROM commands WHERE id=?2),
                                   human_runs = COALESCE(human_runs,0) + (SELECT COALESCE(human_runs,0) FROM commands WHERE id=?2),
                                   agent_runs = COALESCE(agent_runs,0) + (SELECT COALESCE(agent_runs,0) FROM commands WHERE id=?2),
                                   last_used = MAX(COALESCE(last_used,0), (SELECT COALESCE(last_used,0) FROM commands WHERE id=?2))
                                 WHERE id=?1",
                                rusqlite::params![x, id],
                            )?;
                            tx.execute("UPDATE executions SET command_id=? WHERE command_id=?", rusqlite::params![x, id])?;
                            db::delete_command_rows(&tx, &[id])?;
                        }
                        _ => {
                            tx.execute("UPDATE commands SET cmd_text=?, cmd_hash=? WHERE id=?", rusqlite::params![masked, hash, id])?;
                        }
                    }
                }
                tx.execute("DELETE FROM fix_pairs WHERE failed_text=? OR fixed_cmd=?", [orig, orig])?;
                if !masked_texts.contains(masked) {
                    masked_texts.push(masked.clone());
                }
            }
            tx.commit()?;
        }
        self.reload()?;
        // their vectors were computed from the text with the secret in it
        let work: Vec<(String, Vec<i64>)> = {
            let st = self.store.read();
            masked_texts.iter().filter_map(|t| st.entry(t).map(|(_, e)| (t.clone(), e.rows.iter().map(|r| r.id).collect()))).collect()
        };
        self.embed_rows(&work)?;
        // SQLite keeps old copies of changed pages (free pages, the WAL) until it compacts:
        // without this, the secrets would still be in the file
        {
            let conn = self.db.lock();
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM; PRAGMA wal_checkpoint(TRUNCATE);")?;
        }
        self.reload()?;
        out["applied"] = json!(true);
        Ok(out)
    }

    /// One-time upgrade when describe() learns something new (DESCRIBE_VERSION) (v2: see through wrappers like
    /// `docker exec web alembic ...`; v3: two-level tools like `docker compose down`; v4: one
    /// vector per description sentence):
    /// re-describe and re-embed ONLY the affected commands' descriptions, in the background,
    /// instead of asking the user for a full reindex.
    pub fn redescribe(&self) -> Result<usize> {
        let from = db::meta_get(&self.db.lock(), "describe_version")?;
        if from.as_deref() == Some(DESCRIBE_VERSION) {
            return Ok(0);
        }
        let before = |v: u32| from.as_deref().map_or(true, |f| f.parse::<u32>().unwrap_or(0) < v);
        let v1 = before(2);
        // v4 embeds each description sentence on its own: every described command, once
        let every_described = before(4);
        let work: Vec<(String, Vec<i64>)> = {
            let st = self.store.read();
            st.entries
                .iter()
                .filter(|e| {
                    let d = describe::describe(&e.text);
                    e.alive && ((v1 && describe::wrapped(&e.text).is_some()) || (every_described && !d.parts.is_empty()) || d.display != e.desc)
                })
                .map(|e| (e.text.clone(), e.rows.iter().map(|r| r.id).collect()))
                .collect()
        };
        for chunk in work.chunks(128) {
            let descs: Vec<describe::Description> = chunk.iter().map(|(t, _)| describe::describe(t)).collect();
            let batch: Vec<&str> = descs.iter().flat_map(|d| d.parts.iter().map(|p| p.1.as_str())).collect();
            let mut vecs = if batch.is_empty() { Vec::new() } else { self.embedder.embed_many(&batch)? }.into_iter();
            let mut conn = self.db.lock();
            let tx = conn.transaction()?;
            for ((_, ids), d) in chunk.iter().zip(&descs) {
                let parts: Vec<(&str, Vec<f32>)> = d.parts.iter().map(|p| (p.0, vecs.next().unwrap())).collect();
                for (i, id) in ids.iter().enumerate() {
                    db::write_descriptions(&tx, *id, d.display.as_deref(), if i == 0 { &parts } else { &[] })?;
                }
            }
            tx.commit()?;
        }
        db::meta_set(&self.db.lock(), "describe_version", DESCRIBE_VERSION)?;
        if !work.is_empty() {
            self.reload()?;
        }
        Ok(work.len())
    }

    pub fn reload(&self) -> Result<()> {
        let st = {
            let conn = self.db.lock();
            Store::load(&conn)?
        };
        *self.store.write() = st;
        Ok(())
    }

    /// Scrub junk + recompute pass/fail from the real per-run exit codes (Python clean_db).
    fn clean(&self) -> Result<usize> {
        let junk: Vec<i64> = {
            let conn = self.db.lock();
            let mut st = conn.prepare("SELECT id, cmd_text FROM commands")?;
            let rows = st.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?)))?;
            rows.filter_map(|r| r.ok()).filter(|(_, t)| is_junk(t.as_deref().unwrap_or(""))).map(|(i, _)| i).collect()
        };
        {
            let mut conn = self.db.lock();
            let tx = conn.transaction()?;
            db::delete_command_rows(&tx, &junk)?;
            tx.execute_batch(
                "UPDATE commands SET
                   success_count = (SELECT COUNT(*) FROM executions e WHERE e.command_id=commands.id AND e.exit=0),
                   fail_count    = (SELECT COUNT(*) FROM executions e WHERE e.command_id=commands.id AND e.exit>0),
                   last_exit     = (SELECT e.exit FROM executions e WHERE e.command_id=commands.id ORDER BY e.ts DESC LIMIT 1)
                 WHERE EXISTS (SELECT 1 FROM executions e WHERE e.command_id=commands.id)",
            )?;
            tx.commit()?;
        }
        self.reload()?;
        Ok(junk.len())
    }

    /// Double-capture cleanup. While Atuin and the prompt hook both ran, one human run could be
    /// stored twice: Atuin's copy (exit -1 = unknown, stamped at START) and the hook's copy (real
    /// exit, stamped at END). Pair each unknown-exit human run with at most one real-exit human
    /// run of the same text within 60s and drop only the unknown EXECUTION row + its count.
    /// Command rows are never deleted here.
    pub fn fix_dupes(&self) -> Result<usize> {
        const WINDOW: i64 = 60;
        let removed = {
            let mut conn = self.db.lock();
            let tx = conn.transaction()?;
            let runs: Vec<(i64, i64, String, i64, bool)> = {
                let mut st = tx.prepare(
                    "SELECT e.id, e.command_id, c.cmd_text, e.ts, (e.exit IS NULL OR e.exit < 0)
                     FROM executions e JOIN commands c ON c.id = e.command_id
                     WHERE e.ts IS NOT NULL AND COALESCE(e.actor, 'human') NOT LIKE 'agent%'
                     ORDER BY c.cmd_text, e.ts",
                )?;
                let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))?;
                rows.collect::<Result<_, _>>()?
            };
            // greedy one-to-one matching inside each text's time-ordered run list
            let mut used = std::collections::HashSet::new();
            let mut dupes: Vec<(i64, i64)> = Vec::new();
            for (i, (uid, ucid, utext, uts, unknown)) in runs.iter().enumerate() {
                if !unknown {
                    continue;
                }
                let twin = runs[..i]
                    .iter()
                    .rev()
                    .take_while(|r| r.2 == *utext && uts - r.3 <= WINDOW)
                    .chain(runs[i + 1..].iter().take_while(|r| r.2 == *utext && r.3 - uts <= WINDOW))
                    .filter(|r| !r.4 && !used.contains(&r.0))
                    .min_by_key(|r| (r.3 - uts).abs());
                if let Some(k) = twin {
                    used.insert(k.0);
                    dupes.push((*uid, *ucid));
                }
            }
            for (eid, cid) in &dupes {
                tx.execute("DELETE FROM executions WHERE id=?", [eid])?;
                tx.execute(
                    "UPDATE commands SET run_count=MAX(1, COALESCE(run_count,1)-1), human_runs=MAX(0, COALESCE(human_runs,0)-1) WHERE id=?",
                    [cid],
                )?;
            }
            tx.commit()?;
            dupes.len()
        };
        self.reload()?;
        Ok(removed)
    }

    fn stats(&self) -> Value {
        let st = self.store.read();
        let alive: Vec<&crate::store::Entry> = st.entries.iter().filter(|e| e.alive && !e.comment).collect();
        let mut runs = 0u64;
        let (mut ok, mut fail, mut human, mut agent) = (0u64, 0u64, 0u64, 0u64);
        let mut top: Vec<(u32, &str)> = Vec::new();
        let mut failing: Vec<(u32, &str)> = Vec::new();
        for e in &alive {
            let a = st.agg(e, Scope::All);
            runs += a.runs as u64;
            ok += a.ok as u64;
            fail += a.fail as u64;
            human += a.human as u64;
            agent += a.agent as u64;
            top.push((a.runs, &e.text));
            if a.fail > 0 {
                failing.push((a.fail, &e.text));
            }
        }
        top.sort_by(|a, b| b.0.cmp(&a.0));
        failing.sort_by(|a, b| b.0.cmp(&a.0));
        let mut folders: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
        for x in &st.execs {
            if let Some(c) = x.cwd {
                *folders.entry(c).or_default() += 1;
            }
        }
        let mut fv: Vec<(u32, u32)> = folders.into_iter().collect();
        fv.sort_by(|a, b| b.1.cmp(&a.1));
        let pairs: usize = self.fixes.lock().pairs.values().map(|v| v.len()).sum();
        json!({
            "commands": alive.len(), "runs": runs, "executions": st.execs.len(),
            "ok_runs": ok, "failed_runs": fail, "human_runs": human, "agent_runs": agent,
            "fix_pairs": pairs, "pinned": alive.iter().filter(|e| e.pinned).count(),
            "hidden_pasted_code": alive.iter().filter(|e| e.noise).count(),
            "no_folder": alive.iter().filter(|e| e.rows.iter().all(|r| st.row_unscoped(r))).count(),
            "top": top.iter().take(10).map(|(n, t)| json!({"command": t, "runs": n})).collect::<Vec<_>>(),
            "failing": failing.iter().take(5).map(|(n, t)| json!({"command": t, "fails": n})).collect::<Vec<_>>(),
            "folders": fv.iter().take(8).map(|(c, n)| json!({"cwd": st.cwd_name(*c), "runs": n})).collect::<Vec<_>>(),
            "uptime_s": self.started.elapsed().as_secs(),
        })
    }

    pub fn drain_spool(&self) -> Result<usize> {
        let path = config::spool_path();
        if !path.exists() {
            return Ok(0);
        }
        let work = path.with_extension(format!("draining.{}", std::process::id()));
        if std::fs::rename(&path, &work).is_err() {
            return Ok(0); // a hook is appending right now; next tick
        }
        let text = std::fs::read_to_string(&work).unwrap_or_default();
        let runs: Vec<Run> = text
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter_map(|v| run_from(&v, "human"))
            .collect();
        for chunk in runs.chunks(256) {
            self.ingest_runs(chunk)?;
        }
        let _ = std::fs::remove_file(&work);
        if !runs.is_empty() {
            log(&format!("drained {} spooled runs", runs.len()));
        }
        Ok(runs.len())
    }

    pub fn handle(&self, req: &Value) -> Result<Value> {
        let op = s(req, "op").unwrap_or("");
        Ok(match op {
            "ping" => json!({"ok": true, "indexed": self.store.read().alive_count(), "version": config::VERSION,
                              "pid": std::process::id(), "engine": "rust",
                              "descriptions_current": db::meta_get(&self.db.lock(), "describe_version").ok().flatten().as_deref() == Some(DESCRIBE_VERSION)}),
            "ingest" => match run_from(req, "agent:claude-code") {
                None => json!({"ok": false, "skipped": "empty"}),
                Some(mut r) => {
                    // an agent's command: its duration from the PreToolUse start
                    if r.duration_ms.is_none() {
                        if let Some(t0) = s(req, "start_id").and_then(|id| self.agent_starts.lock().remove(id)) {
                            r.duration_ms = Some((now_ms() - t0).max(0));
                        }
                    }
                    let (new, sug) = self.ingest_runs(std::slice::from_ref(&r))?;
                    let mut v = json!({"ok": true, "new": new > 0});
                    if let Some(Some(sg)) = sug.into_iter().next() {
                        v["suggest"] = sg;
                    }
                    // a command that kept working here has just failed: what ran here since
                    // (looked up as stored: a secret in it was masked)
                    // (or that it is flaky here: then nothing broke)
                    if r.failed() {
                        let stored = crate::redact::redact(&r.cmd);
                        if let Some(n) = r.cwd.as_deref().and_then(|c| self.flaky_note(&stored, c).or_else(|| self.broke_note(&stored, c))) {
                            v["note"] = json!(n);
                        }
                        // an agent running it again and again, failing the same way each time
                        if r.is_agent() && !r.session.is_empty() {
                            if let Some(n) = self.retry_loop_note(&stored, db::StreakOf::Session(&r.session)) {
                                v["loop"] = json!(n);
                            }
                        }
                    }
                    v
                }
            },
            "agent_start" => {
                if let Some(id) = s(req, "id") {
                    let mut m = self.agent_starts.lock();
                    let now = now_ms();
                    // commands whose PostToolUse never came don't pile up
                    if m.len() > 1000 {
                        m.retain(|_, t| now - *t < 3_600_000);
                    }
                    m.insert(id.to_string(), now);
                }
                json!({"ok": true})
            }
            "scrub" => self.scrub(b(req, "apply", false))?,
            "ingest_batch" => {
                let runs: Vec<Run> = req.get("items").and_then(Value::as_array).map(|a| a.iter().filter_map(|v| run_from(v, "human")).collect()).unwrap_or_default();
                let (new, _) = self.ingest_runs(&runs)?;
                json!({"ok": true, "ingested": runs.len(), "new": new})
            }
            "search" => self.search_op(req, false)?,
            "recent" => self.search_op(req, true)?,
            "didyoumean" => self.dym_op(req)?,
            "fixfor" => json!({"suggest": self.suggest_for(s(req, "command").unwrap_or(""), s(req, "cwd"), s(req, "error"))}),
            "next" => self.next_op(req),
            "folders" => self.folders_op(req),
            "flow_arm" => {
                let steps: Vec<String> = req["steps"].as_array().map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect()).unwrap_or_default();
                let sess = s(req, "session").unwrap_or("").to_string();
                let pos = (n(req, "pos", 0).max(0) as usize).min(steps.len());
                if sess.is_empty() || steps.is_empty() {
                    json!({"ok": false, "error": "session and steps are required"})
                } else {
                    let total = steps.len();
                    self.flows_armed.lock().insert(sess, ArmedFlow { steps, pos, touched: config::now() });
                    json!({"ok": true, "pos": pos, "total": total})
                }
            }
            "flow_stop" => json!({"ok": true, "stopped": self.flows_armed.lock().remove(s(req, "session").unwrap_or("")).is_some()}),
            "flows" => self.flows_op(req),
            "welcome" => match (s(req, "cwd"), s(req, "session")) {
                (Some(c), Some(sess)) => json!({"line": self.welcome(c, sess)}),
                _ => json!({"line": null}),
            },
            "why" => self.why_op(req),
            "precheck" => self.precheck_op(req),
            "nextup" => self.nextup_op(req),
            "day" => self.day_op(req)?,
            "goto" => self.goto_op(req)?,
            "agents" => self.agents_op(req)?,
            "here" => self.here_op(req),
            "runbook" => self.runbook_op(req),
            "describe" => {
                let st = self.store.read();
                let d = s(req, "command").and_then(|c| st.entry(c)).and_then(|(_, e)| e.desc.clone()).unwrap_or_default();
                json!({"description": d})
            }
            "detail" => {
                let st = self.store.read();
                match s(req, "command").and_then(|c| st.entry(c)) {
                    None => json!({"found": false}),
                    Some((i, e)) => {
                        let mut v = self.item(&st, &search::Hit { idx: i, score: 0.0, sim: -2.0, fuzzy: 0.0, variants: 1, matched_terms: 0, close: true, words: 0.0 }, Scope::All);
                        let mut fl: Vec<(i64, String)> = e.rows.iter().filter_map(|r| r.cwd.map(|c| (r.last_used, st.cwd_name(c).to_string()))).collect();
                        fl.sort_by(|a, b| b.0.cmp(&a.0));
                        v["folder_list"] = json!(fl.into_iter().map(|x| x.1).take(5).collect::<Vec<_>>());
                        v["found"] = json!(true);
                        v
                    }
                }
            }
            "forget" => json!({"ok": true, "removed": self.forget(s(req, "command").unwrap_or(""))?}),
            "pin" => {
                let text = s(req, "command").unwrap_or("");
                let on = b(req, "on", true);
                let ok = self.store.write().set_pinned(text, on);
                if ok {
                    self.db.lock().execute("UPDATE commands SET pinned=? WHERE cmd_text=?", rusqlite::params![on as i64, text])?;
                }
                json!({"ok": ok, "pinned": on})
            }
            "sync" | "import" => {
                let src = s(req, "source").unwrap_or("atuin");
                let r = import::run(self, src, s(req, "path"), n(req, "limit", 0))?;
                json!({"ok": true, "ingested": r.ingested, "new": r.new, "remaining": r.remaining, "source": src})
            }
            "clean" => json!({"ok": true, "removed": self.clean()?}),
            "purge" => json!({"ok": true, "purged": self.purge_failed(n(req, "fail_ttl_days", 1))?}),
            "fix_dupes" => json!({"ok": true, "removed": self.fix_dupes()?}),
            "fixpairs_rebuild" => {
                // one-time backfill: live capture maintains pairs afterwards, and replaying
                // history twice would double every count
                let done = db::meta_get(&self.db.lock(), "fixpairs_backfilled")?.is_some();
                if done && !b(req, "force", false) {
                    let n: usize = self.fixes.lock().pairs.values().map(|v| v.len()).sum();
                    return Ok(json!({"ok": true, "pairs": n, "already": true}));
                }
                let found = fixpairs::rebuild(&self.store.read());
                let mut fx = self.fixes.lock();
                let conn = self.db.lock();
                for (p, cwd, ts) in &found {
                    fx.persist(&conn, p, cwd.as_deref(), *ts)?;
                }
                db::meta_set(&conn, "fixpairs_backfilled", &config::now().to_string())?;
                json!({"ok": true, "pairs": found.len()})
            }
            "stats" => self.stats(),
            "reindex" => json!({"ok": true, "reembedded": self.reindex()?}),
            "mcp" => {
                let ans = mcp::answer(self, s(req, "tool").unwrap_or(""), req.get("args").unwrap_or(&Value::Null), &mcp::Policy::from_req(req))?;
                if b(req, "notes", false) { ans.envelope() } else { ans.value }
            }
            "shutdown" => {
                std::thread::spawn(|| {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    std::process::exit(0)
                });
                json!({"ok": true})
            }
            other => json!({"error": format!("unknown op {other:?}")}),
        })
    }
}

fn serve_conn(d: Arc<Daemon>, stream: TcpStream) {
    let _ = stream.set_nodelay(true);
    let mut w = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let mut r = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        match r.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Value>(line.trim()) {
            // needs the Arc (the listener outlives this connection), so it's handled here
            Ok(req) if req.get("op").and_then(Value::as_str) == Some("http_enable") => match crate::settings::load().http {
                None => json!({"ok": false, "error": "http endpoint not configured"}),
                Some(h) => match crate::http::start(d.clone(), h.port) {
                    Ok(started) => json!({"ok": true, "started": started, "port": h.port}),
                    Err(e) => json!({"ok": false, "error": format!("port {}: {e}", h.port)}),
                },
            },
            Ok(req) => d.handle(&req).unwrap_or_else(|e| json!({"error": e.to_string()})),
            Err(e) => json!({"error": format!("bad json: {e}")}),
        };
        let mut out = serde_json::to_vec(&resp).unwrap_or_default();
        out.push(b'\n');
        if w.write_all(&out).and_then(|_| w.flush()).is_err() {
            break;
        }
    }
}

pub fn serve(port: u16) -> Result<()> {
    std::fs::create_dir_all(config::home())?;
    let t = Instant::now();
    let listener = TcpListener::bind((config::HOST, port)).with_context(|| format!("port {port} busy - is a daemon already running?"))?;
    let d = Arc::new(Daemon::open()?);
    let purged = d.purge_failed(1)?;
    let drained = d.drain_spool().unwrap_or(0);
    if let Some(h) = crate::settings::load().http {
        if let Err(e) = crate::http::start(d.clone(), h.port) {
            log(&format!("http endpoint on port {} failed: {e}", h.port));
        }
    }
    let msg = format!(
        "reman daemon (rust) warm: {} commands indexed in {:.2}s, purged {purged}, drained {drained}, listening on {}:{port}",
        d.store.read().alive_count(),
        t.elapsed().as_secs_f32(),
        config::HOST
    );
    log(&msg);
    println!("{msg}");
    {
        let d = d.clone();
        std::thread::spawn(move || match d.redescribe() {
            Ok(0) => {}
            Ok(n) => log(&format!("re-described {n} commands whose description improved")),
            Err(e) => log(&format!("redescribe failed: {e}")),
        });
    }
    {
        let d = d.clone();
        std::thread::spawn(move || {
            let mut tick = 0u64;
            loop {
                // 1s: fish (no TCP builtin) appends successes to the spool instead of spawning a
                // process, so the spool is a live capture path; an absent file is one stat()
                std::thread::sleep(std::time::Duration::from_secs(1));
                tick += 1;
                if let Err(e) = d.drain_spool() {
                    log(&format!("spool drain failed: {e}"));
                }
                if tick % 3600 == 0 {
                    let _ = d.purge_failed(1); // hourly retention
                }
            }
        });
    }
    for conn in listener.incoming() {
        match conn {
            Ok(s) => {
                let d = d.clone();
                std::thread::spawn(move || serve_conn(d, s));
            }
            Err(e) => log(&format!("accept failed: {e}")),
        }
    }
    Ok(())
}

#[cfg(test)]
mod flow_tests {
    use super::ArmedFlow;

    #[test]
    fn a_flow_advances_on_its_own_steps_only() {
        let mut f = ArmedFlow { steps: vec!["git pull".into(), "npm i".into(), "npm run dev".into()], pos: 0, touched: 0 };
        f.observe("ls", true, 1);
        assert_eq!(f.pos, 0); // unrelated commands don't move it
        f.observe("git pull", false, 2);
        assert_eq!(f.pos, 0); // a failed step is still the next step
        f.observe("git pull", true, 3);
        assert_eq!(f.pos, 1);
        f.observe("npm run dev", true, 4); // skipping ahead is fine
        assert_eq!(f.pos, 3);
    }

    #[test]
    fn windows_exit_codes() {
        use super::exit_code;
        assert_eq!(exit_code(0), 0);
        assert_eq!(exit_code(2), 2);
        // npm on Windows with no package.json (ENOENT), a crash (0xC0000005): real failures
        assert_eq!(exit_code(-4058), 0xFFFF_F026);
        assert_eq!(exit_code(-1073741819), 0xC000_0005);
        assert!(exit_code(-1) > 0);
    }

    #[test]
    fn a_terminals_own_startup_line_is_not_a_command() {
        use super::is_junk;
        assert!(is_junk(r#"try { . "c:\Users\me\AppData\Local\Programs\Microsoft VS Code\04c0d99f4f\resources\app\out\vs\workbench\contrib\terminal\common\scripts\shellIntegration.ps1" } catch {}"#));
        assert!(is_junk(r#". "/usr/share/code/resources/app/out/vs/workbench/contrib/terminal/common/scripts/shellIntegration-bash.sh""#));
        assert!(!is_junk("npm test"));
    }
}
