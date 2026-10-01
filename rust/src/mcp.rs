//! MCP agent interface. Two halves:
//!  - `answer` runs INSIDE the daemon (warm model, in-memory history), enforcing the security
//!    boundary + secret redaction from the policy the bridge sends.
//!  - `serve_stdio` is the `reman mcp` process an agent launches: a small JSON-RPC 2.0 stdio server
//!    that forwards tool calls to the daemon.
//! HARD CONTRACT (unchanged from the Python server): every command returned is one the user
//! really ran - never generated - and nothing outside REMAN_MCP_ROOT is ever returned.
use crate::client::Client;
use crate::config;
use crate::daemon::Daemon;
use crate::dym;
use crate::flows;
use crate::predict;
use crate::redact;
use crate::search::{self, Query, Rank};
use crate::store::{Entry, Row, Scope, Store};
use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::io::{BufRead, Write};

#[derive(Debug, Clone)]
pub struct Policy {
    pub roots: Vec<String>,
    pub allow_global: bool,
    pub strict: bool,
    /// also share generic commands that have no recorded folder (settings.share_old_history)
    pub old_history: bool,
    /// the project the agent works in (the repo it launched us in). Visible only when the user
    /// shared it; then its commands rank first. When not, answers say how to ask for it.
    pub project: Option<String>,
}

/// The project the agent launched us in: the repo around that folder (a git worktree counts as
/// the repo it belongs to), else the folder itself - unless that is the home folder or a drive
/// root, which would be everything; then there is none.
fn project_root() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    project_of(&cwd)
}

pub(crate) fn project_of(cwd: &std::path::Path) -> Option<String> {
    let home = dirs::home_dir().map(|h| config::norm_path(&h.to_string_lossy()));
    let too_wide = |p: &std::path::Path| p.parent().is_none() || home.as_deref() == Some(config::norm_path(&p.to_string_lossy()).as_str());
    let mut root = cwd.to_path_buf();
    for dir in cwd.ancestors().take_while(|d| !too_wide(d)) {
        let git = dir.join(".git");
        if git.is_dir() {
            root = dir.to_path_buf();
            break;
        }
        if git.is_file() {
            // a worktree: `gitdir: <repo>/.git/worktrees/<name>`
            let main = std::fs::read_to_string(&git).ok().and_then(|t| {
                let gd = t.trim().strip_prefix("gitdir:")?.trim().replace('\\', "/");
                gd.find("/.git/worktrees/").map(|i| std::path::PathBuf::from(&gd[..i]))
            });
            root = main.filter(|m| m.is_dir()).unwrap_or_else(|| dir.to_path_buf());
            break;
        }
    }
    (!too_wide(&root)).then(|| config::norm_path(&root.to_string_lossy()))
}

impl Policy {
    /// Server config comes from the MCP process env - never from tool arguments (an agent can pass
    /// any cwd, so cwd is untrusted input).
    /// Roots: $REMAN_MCP_ROOT (per agent), else ~/.reman/config.json `mcp_roots`. Both are the
    /// user's own choice: nothing is visible to an agent until the user approves a folder, not
    /// even the project the agent works in.
    pub fn from_env() -> Self {
        let sep = if cfg!(windows) { ';' } else { ':' };
        let st = crate::settings::load();
        let roots: Vec<String> = match std::env::var("REMAN_MCP_ROOT").ok().filter(|s| !s.trim().is_empty()) {
            Some(raw) => raw.split(sep).filter(|p| !p.trim().is_empty()).map(config::norm_path).collect(),
            None => st.mcp_roots.iter().map(|r| config::norm_path(r)).collect(),
        };
        Self {
            project: project_root(),
            roots,
            allow_global: std::env::var("REMAN_MCP_ALLOW_GLOBAL").as_deref() == Ok("1"),
            strict: std::env::var("REMAN_MCP_STRICT_SECRETS").map(|v| v == "1").unwrap_or(st.strict_secrets),
            old_history: std::env::var("REMAN_MCP_OLD_HISTORY").map(|v| v == "1").unwrap_or(st.share_old_history),
        }
    }

    /// The HTTP endpoint has no per-agent env: config.json only (no roots configured = sees nothing).
    pub fn from_settings() -> Self {
        let st = crate::settings::load();
        Self { roots: st.mcp_roots.iter().map(|r| config::norm_path(r)).collect(), allow_global: false, strict: st.strict_secrets, old_history: st.share_old_history, project: None }
    }

    pub fn from_req(req: &Value) -> Self {
        Self {
            roots: req.get("roots").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(config::norm_path).collect()).unwrap_or_default(),
            allow_global: req.get("allow_global").and_then(Value::as_bool).unwrap_or(false),
            strict: req.get("strict").and_then(Value::as_bool).unwrap_or(false),
            old_history: req.get("old_history").and_then(Value::as_bool).unwrap_or(false),
            project: req.get("project").and_then(Value::as_str).map(config::norm_path),
        }
    }

    pub fn within(&self, path: Option<&str>) -> bool {
        if self.allow_global {
            return true;
        }
        let Some(p) = path.filter(|p| !p.is_empty()) else { return false };
        let p = config::norm_path(p);
        let sep = std::path::MAIN_SEPARATOR;
        self.roots.iter().any(|r| p == *r || p.starts_with(&format!("{r}{sep}")))
    }

    /// Agents always get secrets masked, even when the user keeps them as typed for themselves.
    fn safe(&self, cmd: &str) -> Option<String> {
        redact::safe_command(cmd, self.strict)
    }
}

fn folder_eq(a: Option<&str>, b: &str) -> bool {
    a.is_some_and(|a| config::norm_path(a) == config::norm_path(b))
}

/// Rows of an entry the agent may see (inside the root, and in `cwd` when given), newest first.
/// Rows with no recorded folder are only shared when the policy opts in, the agent didn't ask
/// for a specific folder, and the command itself is project-neutral (describe::is_generic).
fn visible<'a>(st: &Store, e: &'a Entry, pol: &Policy, cwd: Option<&str>) -> Vec<&'a Row> {
    let mut generic: Option<bool> = None;
    let mut rows: Vec<&Row> = e
        .rows
        .iter()
        .filter(|r| {
            if st.row_unscoped(r) && !pol.allow_global {
                return pol.old_history && cwd.is_none() && *generic.get_or_insert_with(|| crate::describe::is_generic(&e.text));
            }
            let c = r.cwd.map(|c| st.cwd_name(c));
            pol.within(c) && cwd.is_none_or(|w| folder_eq(c, w))
        })
        .collect();
    rows.sort_by(|a, b| b.last_used.cmp(&a.last_used));
    rows
}

struct Seen {
    runs: u32,
    ok: u32,
    fail: u32,
    human: u32,
    agent: u32,
    last_used: i64,
    last_exit: Option<i64>,
    actors: Vec<String>,
    cwd: Option<String>,
}

fn sum(st: &Store, rows: &[&Row]) -> Seen {
    Seen {
        runs: rows.iter().map(|r| r.runs).sum(),
        ok: rows.iter().map(|r| r.ok).sum(),
        fail: rows.iter().map(|r| r.fail).sum(),
        human: rows.iter().map(|r| r.human).sum(),
        agent: rows.iter().map(|r| r.agent).sum(),
        last_used: rows.first().map(|r| r.last_used).unwrap_or(0),
        last_exit: rows.first().and_then(|r| r.last_exit),
        actors: rows.iter().map(|r| r.last_actor.clone().unwrap_or_else(|| "human".into())).collect(),
        // newest row that has a real folder; null = only known from folder-less old history
        cwd: rows.iter().find(|r| !st.row_unscoped(r)).and_then(|r| r.cwd).map(|c| st.cwd_name(c).to_string()),
    }
}

fn arg_s<'a>(a: &'a Value, k: &str) -> Option<&'a str> {
    a.get(k).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn arg_n(a: &Value, k: &str, d: i64) -> usize {
    a.get(k).and_then(Value::as_i64).unwrap_or(d).clamp(1, 500) as usize
}

/// A tool's answer plus, when the answer alone would mislead (empty, or filtered), one line on why.
pub struct Answer {
    pub value: Value,
    pub note: Option<String>,
}

impl Answer {
    fn plain(value: Value) -> Self {
        Self { value, note: None }
    }

    /// {"result", "note"}: what the daemon sends a bridge that asked for notes
    pub fn envelope(self) -> Value {
        json!({"result": self.value, "note": self.note})
    }

    /// back from an envelope (an older daemon sends the bare result)
    pub fn from_reply(v: Value) -> Self {
        match v {
            Value::Object(mut m) if m.len() == 2 && m.contains_key("result") && m.contains_key("note") => {
                let note = m.remove("note").and_then(|n| n.as_str().map(String::from));
                Self { value: m.remove("result").unwrap_or(Value::Null), note }
            }
            v => Self::plain(v),
        }
    }
}

fn share_hint(folder: &str) -> String {
    format!(
        "Only the user can approve it: call {SHARE_TOOL} and your app asks them (for this session), or they run `reman connect --add-root \"{folder}\"` in their own terminal to share it for good. Do not run that yourself."
    )
}

pub fn answer(d: &Daemon, name: &str, args: &Value, pol: &Policy) -> Result<Answer> {
    let mut ans = answer_inner(d, name, args, pol)?;
    // the agent's own project, not approved: every answer says so, since it explains the gaps
    if let Some(p) = pol.project.as_deref().filter(|p| arg_s(args, "cwd").is_none() && !pol.within(Some(p))) {
        let line = format!("This project ({p}) is not shared with agents, so its history is hidden. {}", share_hint(p));
        ans.note = Some(match ans.note {
            Some(n) => format!("{line} {n}"),
            None => line,
        });
    }
    Ok(ans)
}

fn answer_inner(d: &Daemon, name: &str, args: &Value, pol: &Policy) -> Result<Answer> {
    let cwd = arg_s(args, "cwd");
    if cwd.is_some() && !pol.within(cwd) {
        let c = cwd.unwrap_or("");
        let note = format!("{c} is not a folder this user shares with agents, so nothing from it is shown (this is not proof nothing ran there). {}", share_hint(c));
        return Ok(if name == "reman_check" {
            Answer::plain(json!({"command": arg_s(args, "command").unwrap_or(""), "verdict": "denied",
                   "advice": format!("That folder is outside the folders shared with this agent. {}", share_hint(c)), "generated": false}))
        } else {
            Answer { value: json!([]), note: Some(note) }
        });
    }
    let value = match name {
        "reman_search" => return tool_search(d, args, pol, cwd),
        "reman_recent" | "reman_failures" => {
            let v = tool_recent(d, args, pol, cwd, name == "reman_failures");
            let empty = v.as_array().is_some_and(|a| a.is_empty());
            let place = cwd.map(|c| format!(" in {c}")).unwrap_or_else(|| " in the folders shared with this agent".into());
            let note = empty.then(|| {
                if name == "reman_failures" { format!("No command that only ever failed is recorded{place}.") } else { format!("No commands are recorded{place}.") }
            });
            return Ok(Answer { value: v, note });
        }
        other => call_one(d, other, args, pol, cwd)?,
    };
    Ok(Answer::plain(value))
}

fn call_one(d: &Daemon, name: &str, args: &Value, pol: &Policy, cwd: Option<&str>) -> Result<Value> {
    match name {
        "reman_fixes" => tool_fixes_for(d, arg_s(args, "failed_command").unwrap_or(""), arg_s(args, "error"), pol, cwd, 3),
        "reman_check" => tool_check(d, arg_s(args, "command").unwrap_or(""), pol, cwd),
        "reman_flows" => Ok(tool_flows(d, args, pol)),
        "reman_next" => Ok(tool_next(d, args, pol, cwd)),
        "reman_runbook" => Ok(tool_runbook(d, pol, cwd)),
        // the stdio server answers it itself (Consent); reaching the daemon means no app to approve it
        SHARE_TOOL => Ok(json!({"shared": false, "reason": "Sharing a folder needs the user's approval in an app's MCP session. Over HTTP, the user shares folders in `reman settings` or with `reman connect --add-root`.", "generated": false})),
        other => Err(anyhow!("unknown tool {other}")),
    }
}

/// Is any of these rows in `folder` or below it?
fn rows_in(st: &Store, rows: &[&Row], folder: &str) -> bool {
    let sep = std::path::MAIN_SEPARATOR;
    rows.iter().filter_map(|r| r.cwd).map(|c| config::norm_path(st.cwd_name(c))).any(|c| c == folder || c.starts_with(&format!("{folder}{sep}")))
}

/// how much a command from the agent's own project gains in reman_search's order (on top of search's
/// W_HERE for the exact folder): enough to win a near tie, not to beat a clearly better match
const HERE_BONUS: f32 = 0.02;

fn who_ran(s: &Seen) -> &'static str {
    match (s.human > 0, s.agent > 0) {
        (true, true) => "human and agent",
        (false, true) => "agent",
        _ => "human",
    }
}

fn tool_search(d: &Daemon, a: &Value, pol: &Policy, cwd: Option<&str>) -> Result<Answer> {
    // a time named at the end (`deploy last week`) filters to what ran then: still only real runs
    let (words, window) = crate::timewords::split(arg_s(a, "intent").unwrap_or(""), config::now(), d.local_offset());
    let intent = words.as_str();
    let k = arg_n(a, "k", 5);
    let worked = a.get("worked_only").and_then(Value::as_bool).unwrap_or(true);
    let prefer_human = arg_s(a, "prefer") == Some("human");
    let qv = if intent.trim().chars().count() >= 3 { Some(d.embedder.embed_query(intent.trim())?) } else { None };
    let st = d.store.read();
    // the folder asked about, else the agent's own project (when shared): its commands come first
    let home: Option<String> = cwd.map(config::norm_path).or_else(|| pol.project.clone().filter(|p| pol.within(Some(p))));
    let q = Query { text: intent, k: 400, offset: 0, scope: Scope::All, actor: None, status: None, group: true, rank: Rank::Hybrid, here: home.as_deref().and_then(|c| st.cwd_index(c)), window: window.map(|w| (w.since, w.until)) };
    let out = search::search(&st, &q, qv.as_deref().map(|v| v.as_slice()));
    let mut res: Vec<(Value, u32, f32)> = Vec::new();
    let mut failed_only = 0;
    // relevance floor: an empty answer ("nothing known") beats the least-bad unrelated command
    for h in out.hits.iter().filter(|h| h.close) {
        let e = &st.entries[h.idx as usize];
        let rows = visible(&st, e, pol, cwd);
        if rows.is_empty() {
            continue;
        }
        let Some(safe) = pol.safe(&e.text) else { continue };
        let s = sum(&st, &rows);
        // worked_only drops what is known to fail, never what simply has no recorded outcome
        if worked && s.fail > 0 && s.ok == 0 {
            failed_only += 1;
            continue;
        }
        let here = home.as_deref().is_some_and(|f| rows_in(&st, &rows, f));
        let v = if out.mode == "manual" {
            json!({"command": safe, "run_count": s.runs, "match": "literal", "generated": false})
        } else {
            let mut v = json!({"command": safe, "cwd": s.cwd, "run_count": s.runs, "intent": e.gkey,
                "variants": h.variants, "similarity": (h.sim.max(0.0) * 1000.0).round() / 1000.0,
                "description": e.desc, "generated": false,
                "success_rate": (s.ok + s.fail > 0).then(|| ((s.ok as f64 / (s.ok + s.fail) as f64) * 100.0).round() / 100.0),
                "last_run": config::age(s.last_used), "actor": who_ran(&s), "human_runs": s.human, "agent_runs": s.agent});
            if here {
                v["in_this_project"] = json!(true);
            }
            if out.mode == "fuzzy" {
                v["match"] = json!("fuzzy");
            }
            if s.cwd.is_none() {
                v["cwd_note"] = json!("from old history - the folder it ran in was never recorded");
            }
            v
        };
        res.push((v, s.human, h.score + if here { HERE_BONUS } else { 0.0 }));
        if res.len() >= k * 3 {
            break;
        }
    }
    // (when asked) what the user ran over what only agents ran, then by score with this project's
    // commands lifted: they win among comparable matches, not over a much better one
    res.sort_by(|a, b| (prefer_human && a.1 == 0).cmp(&(prefer_human && b.1 == 0)).then(b.2.total_cmp(&a.2)));
    let mut notes: Vec<String> = Vec::new();
    if res.is_empty() {
        notes.push(format!("Nothing close to \"{intent}\" in the history this agent can see. Do not take a loosely similar command as the answer; check the project's own scripts and docs."));
        // what the agent's folder does have that is loosely related, marked as such
        if let Some(f) = home.as_deref() {
            let loose: Vec<String> = out
                .hits
                .iter()
                .filter(|h| !h.close)
                .filter_map(|h| {
                    let e = &st.entries[h.idx as usize];
                    let rows = visible(&st, e, pol, cwd);
                    (e.text.len() <= 80 && !e.text.contains('\n') && rows_in(&st, &rows, f)).then(|| pol.safe(&e.text)).flatten()
                })
                .take(3)
                .collect();
            if !loose.is_empty() {
                notes.push(format!("Loosely related, from {f} (not matches): {}.", loose.iter().map(|c| format!("`{c}`")).collect::<Vec<_>>().join(", ")));
            }
        }
    }
    if failed_only > 0 {
        notes.push(format!("{failed_only} close match(es) left out because they only ever failed (worked_only); pass worked_only=false to see them."));
    }
    let note = (!notes.is_empty()).then(|| notes.join(" "));
    Ok(Answer { value: Value::Array(res.into_iter().take(k).map(|x| x.0).collect()), note })
}

fn tool_recent(d: &Daemon, a: &Value, pol: &Policy, cwd: Option<&str>, failures: bool) -> Value {
    let n = arg_n(a, "n", 20);
    let st = d.store.read();
    let mut items: Vec<(i64, Value)> = Vec::new();
    for e in st.entries.iter().filter(|e| e.recallable(false)) {
        let rows = visible(&st, e, pol, cwd);
        if rows.is_empty() {
            continue;
        }
        let s = sum(&st, &rows);
        if failures && !(s.fail > 0 && s.ok == 0) {
            continue;
        }
        let Some(safe) = pol.safe(&e.text) else { continue };
        let v = if failures {
            json!({"command": safe, "cwd": s.cwd, "last_exit": s.last_exit, "fail_count": s.fail,
                   "last_run": config::age(s.last_used), "generated": false})
        } else {
            json!({"command": safe, "cwd": s.cwd, "run_count": s.runs, "last_run": config::age(s.last_used), "generated": false})
        };
        items.push((s.last_used, v));
    }
    items.sort_by(|a, b| b.0.cmp(&a.0));
    Value::Array(items.into_iter().take(n).map(|x| x.1).collect())
}

/// reman_fixes with the error the command printed: first what fixed a DIFFERENT command that
/// failed with the same error, then the usual fixes for this one.
fn tool_fixes_for(d: &Daemon, failed: &str, error: Option<&str>, pol: &Policy, cwd: Option<&str>, k: usize) -> Result<Value> {
    let mut out: Vec<Value> = Vec::new();
    if let Some(sig) = error.and_then(crate::errors::signature) {
        let st = d.store.read();
        if let Some((other, fix)) = d.fix_for_error(&st, &sig, failed) {
            let seen = |t: &str| st.entry(t).is_some_and(|(_, e)| !visible(&st, e, pol, cwd).is_empty());
            if seen(&fix) && seen(&other) {
                if let (Some(f), Some(o)) = (pol.safe(&fix), pol.safe(&other)) {
                    out.push(json!({"fixed_command": f, "proven": true, "same_error": true, "was_fixing": o,
                                    "note": "Another command failed with this same error; this is what fixed it. Adapt it to your command.", "generated": false}));
                }
            }
        }
    }
    for v in tool_fixes(d, failed, pol, cwd, k)?.as_array().into_iter().flatten() {
        if out.len() < k && !out.iter().any(|o| o["fixed_command"] == v["fixed_command"]) {
            out.push(v.clone());
        }
    }
    Ok(Value::Array(out))
}

fn tool_fixes(d: &Daemon, failed: &str, pol: &Policy, cwd: Option<&str>, k: usize) -> Result<Value> {
    let qv = d.embedder.embed_query(failed)?;
    let st = d.store.read();
    let mut out: Vec<Value> = Vec::new();
    let ok_rows = |text: &str| st.entry(text).map(|(_, e)| !visible(&st, e, pol, cwd).is_empty()).unwrap_or(false);
    for p in d.fixes.lock().lookup(failed, &st) {
        if out.len() >= k || !ok_rows(&p.fixed) {
            continue;
        }
        let Some(safe) = pol.safe(&p.fixed) else { continue };
        let mut v = json!({"fixed_command": safe, "confidence": 1.0, "proven": true, "fix_type": p.confidence,
                           "times_fixed": p.count, "generated": false});
        if let Some(c) = crate::fixpairs::diff(failed, &safe) {
            v["what_changed"] = json!(c);
        }
        out.push(v);
    }
    for sg in dym::did_you_mean(&st, failed, Some(&qv), 20, false, Scope::All) {
        if out.len() >= k {
            break;
        }
        let text = &st.entries[sg.idx as usize].text;
        if !ok_rows(text) {
            continue;
        }
        let Some(safe) = pol.safe(text) else { continue };
        if out.iter().any(|v| v["fixed_command"] == json!(safe)) {
            continue;
        }
        let mut v = json!({"fixed_command": safe, "confidence": (sg.score * 1000.0).round() / 1000.0,
                           "typo_sim": (sg.typo * 1000.0).round() / 1000.0, "intent_sim": (sg.sem * 1000.0).round() / 1000.0,
                           "proven": false, "generated": false});
        if let Some(c) = crate::fixpairs::diff(failed, &safe) {
            v["what_changed"] = json!(c);
        }
        out.push(v);
    }
    Ok(Value::Array(out))
}

fn tool_check(d: &Daemon, command: &str, pol: &Policy, cwd: Option<&str>) -> Result<Value> {
    let cmd = command.trim();
    let here = if cwd.is_some() { " in this folder" } else { "" };
    let found = {
        let st = d.store.read();
        st.entry(cmd).map(|(_, e)| {
            let rows = visible(&st, e, pol, cwd);
            let folders: Vec<String> = {
                let mut f: Vec<String> = rows.iter().filter_map(|r| r.cwd.map(|c| st.cwd_name(c).to_string())).collect();
                f.sort();
                f.dedup();
                f
            };
            (sum(&st, &rows), folders, rows.len())
        })
    };
    let Some((s, folders, nrows)) = found.filter(|f| f.2 > 0) else {
        let mut similar: Vec<Value> = tool_fixes(d, cmd, pol, cwd, 3)?.as_array().cloned().unwrap_or_default();
        if similar.is_empty() && cwd.is_some() {
            similar = tool_fixes(d, cmd, pol, None, 3)?.as_array().cloned().unwrap_or_default();
        }
        let sim: Vec<Value> = similar.iter().map(|v| v["fixed_command"].clone()).collect();
        let advice = if sim.is_empty() { format!("You have never run this{here}, and nothing similar is known.") } else { format!("You have never run this{here}. Prefer a known command below.") };
        return Ok(json!({"command": pol.safe(cmd).unwrap_or_default(), "verdict": "never_run", "run_count": 0,
                         "advice": advice, "similar": sim, "generated": false}));
    };
    let _ = nrows;
    let (rc, sc, fc) = (s.runs, s.ok, s.fail);
    let last_exit = s.last_exit.map(|e| e.to_string()).unwrap_or_else(|| "?".into());
    let (verdict, advice) = match (sc > 0, fc > 0) {
        (true, false) => ("verified", format!("You've run this {rc}x and it succeeded ({sc} ok){here}. Safe to reuse.")),
        (false, true) => ("failed", format!("This only ever FAILED for you ({fc}x, last exit {last_exit}){here}. Fix or avoid.")),
        (true, true) => ("mixed", format!("Mixed: {sc} ok / {fc} fail across {rc} runs{here}. Use with caution.")),
        _ => ("ran_unknown", format!("You've run this {rc}x{here} but exit codes weren't captured - outcome unknown.")),
    };
    let actor = if s.actors.iter().all(|a| a.starts_with("agent")) {
        "agent"
    } else if s.actors.iter().all(|a| !a.starts_with("agent")) {
        "human"
    } else {
        "mixed"
    };
    let mut out = json!({"command": pol.safe(cmd).unwrap_or_default(), "verdict": verdict, "advice": advice, "run_count": rc,
        "success_count": sc, "fail_count": fc, "last_exit": s.last_exit, "last_run": config::age(s.last_used),
        "actor": actor, "generated": false});
    if cwd.is_none() {
        out["folders"] = json!(folders);
    }
    // how long it usually takes when it works (here, else anywhere): an agent's timeout
    let typical = {
        let st = d.store.read();
        st.entry(cmd).and_then(|(_, e)| {
            let here = cwd.map(|c| st.scope_folder(c)).filter(|s| *s != Scope::Nothing);
            here.and_then(|s| st.typical(e, s, true)).or_else(|| st.typical(e, Scope::All, true))
        })
    };
    if let Some(t) = typical.filter(|t| *t >= 1000) {
        let took = crate::insight::took(t, false);
        out["typical_duration"] = json!(took);
        if t >= 30_000 && verdict != "failed" {
            out["advice"] = json!(format!("{} It usually takes {took}: allow for that before deciding it hung.", out["advice"].as_str().unwrap_or("")));
        }
    }
    // it used to work here and has just started failing: what ran in this folder in between.
    // Or it is flaky here: then a failure says nothing about the agent's change
    if verdict == "mixed" {
        if let Some(c) = cwd {
            let st = d.store.read();
            if let (Some(ci), Some((i, _))) = (st.cwd_index(c), st.entry(cmd)) {
                if let Some(f) = crate::insight::flaky(&st, i, ci) {
                    out["flaky"] = json!({"worked": f.worked, "of_last_runs": f.runs});
                    out["advice"] = json!(format!("This command is flaky here: {}. If it fails, run it again once before changing code: the failure may not be caused by your change.", f.summary()));
                } else if let Some(b) = crate::insight::what_broke(&st, i, ci) {
                    let between: Vec<String> = b
                        .between
                        .iter()
                        .map(|&j| &st.entries[j as usize])
                        .filter(|e| !visible(&st, e, pol, None).is_empty())
                        .filter_map(|e| pol.safe(&e.text))
                        .collect();
                    out["stopped_working"] = json!({"worked_before": b.worked, "last_worked": config::age(b.last_ok), "ran_here_since": between});
                    out["advice"] = json!(format!("This worked {} times here and has just started failing. Look at what ran here since it last worked (ran_here_since) before changing the command.", b.worked));
                    if let Some(g) = crate::insight::GitChange::of(&st, &b) {
                        out["stopped_working"]["git"] = json!(g.clause());
                        out["advice"] = json!(format!("{} {}", out["advice"].as_str().unwrap_or(""), g.advice()));
                    }
                }
            }
        }
    }
    if verdict == "failed" || verdict == "mixed" {
        // what it printed the last time it failed, masked again: stored as typed when the user
        // keeps secrets, but never handed to an agent that way
        let last = {
            let st = d.store.read();
            st.entry(cmd).and_then(|(i, _)| st.last_err.get(&i).cloned()).and_then(|e| pol.safe(&e))
        };
        if let Some(e) = last {
            out["last_error"] = json!(e);
        }
        let fixes = tool_fixes(d, cmd, pol, cwd, 1)?;
        if let Some(f) = fixes.as_array().and_then(|a| a.first()).filter(|f| f["proven"] == json!(true)) {
            out["known_fix"] = f["fixed_command"].clone();
        }
        // agents running it here again and again, failing the same way each time (an agent's
        // session isn't known here: the last half hour of agents' runs in this folder)
        if let Some(c) = cwd {
            const WINDOW_MIN: i64 = 30;
            let of = crate::db::StreakOf::AgentsIn { cwd: c, since: config::now() - WINDOW_MIN * 60 };
            let n = crate::db::same_failures(&d.db.lock(), &crate::redact::redact(cmd), of).map(|r| r.0).unwrap_or(0);
            if n >= 3 {
                out["retry_loop"] = json!({"failed_the_same_way": n, "within_minutes": WINDOW_MIN});
                out["advice"] = json!(format!("Agents ran this here {n} times in a row in the last {WINDOW_MIN} minutes and it failed the same way each time. Running it again unchanged will fail again: change something first, or ask the user."));
            }
        }
    }
    Ok(out)
}

fn tool_flows(d: &Daemon, a: &Value, pol: &Policy) -> Value {
    let n = arg_n(a, "n", 15);
    let st = d.store.read();
    let mut out = Vec::new();
    for f in flows::detect(&st, None, 2, 4, 300, 120) {
        let steps: Vec<&Entry> = f.seq.iter().map(|&i| &st.entries[i as usize]).collect();
        // workflows cross subfolders: every step must have run somewhere inside the root
        if !steps.iter().all(|e| !visible(&st, e, pol, None).is_empty()) {
            continue;
        }
        let safe: Option<Vec<String>> = steps.iter().map(|e| pol.safe(&e.text)).collect();
        let Some(safe) = safe else { continue };
        out.push(json!({"sequence": safe, "count": f.count, "length": f.seq.len(), "generated": false}));
        if out.len() >= n {
            break;
        }
    }
    Value::Array(out)
}

/// How this project is run: per task (set up, run, test, lint, build, database, deploy) the
/// commands that worked, and the usual sequences. For `cwd`, else the project the agent runs in.
fn tool_runbook(d: &Daemon, pol: &Policy, cwd: Option<&str>) -> Value {
    let Some(folder) = cwd.map(str::to_string).or_else(|| pol.roots.first().cloned()) else {
        return json!({"found": false, "reason": "No project folder: pass cwd.", "generated": false});
    };
    let st = d.store.read();
    let fi = st.cwd_index(&folder);
    let scope = match st.scope_repo(&folder) {
        Scope::Nothing => fi.map(Scope::Folder).unwrap_or(Scope::Nothing),
        r => r,
    };
    if scope == Scope::Nothing {
        return json!({"found": false, "reason": "No history in this folder yet.", "generated": false});
    }
    // only what this agent may see, redacted: a recorded command by its rows, one a project file
    // declares by the folder holding that file
    let show = |i: Option<u32>, t: &str, dir: &str| {
        let ok = match i {
            Some(i) => !visible(&st, &st.entries[i as usize], pol, None).is_empty(),
            None => pol.within(Some(dir)),
        };
        if ok { pol.safe(t) } else { None }
    };
    let seen = crate::insight::runbook_json(&st, scope, &folder, &show, true);
    // a model's fuller version when one was written; else this one now, and the fuller one in
    // the background for next time (an agent never waits on a local model)
    let full = crate::insight::runbook_json(&st, scope, &folder, &|_, t, _| Some(t.to_string()), true);
    drop(st);
    match crate::ai::cached(&full) {
        Some((x, model)) => crate::ai::merge(&seen, &x, &model),
        None => {
            crate::ai::generate_in_background(full);
            crate::ai::without_unplaced(seen)
        }
    }
}

fn tool_next(d: &Daemon, a: &Value, pol: &Policy, cwd: Option<&str>) -> Value {
    let n = arg_n(a, "n", 5);
    let st = d.store.read();
    let ci = cwd.and_then(|c| st.cwd_index(c));
    let (last, prev) = match arg_s(a, "last_command") {
        Some(lc) => (st.entry(lc).map(|e| e.0), None),
        None => predict::context(&st, ci, None),
    };
    let mut out = Vec::new();
    for p in predict::predict(&st, ci, last, prev, n * 3) {
        let e = &st.entries[p.idx as usize];
        if visible(&st, e, pol, None).is_empty() {
            continue;
        }
        let Some(safe) = pol.safe(&e.text) else { continue };
        out.push(json!({"command": safe, "reason": p.reason, "count": p.count, "generated": false}));
        if out.len() >= n {
            break;
        }
    }
    Value::Array(out)
}

// ---------------------------------------------------------------------------------------------
// JSON-RPC (shared by the stdio server and the HTTP endpoint)
// ---------------------------------------------------------------------------------------------

/// The MCP tool list (name, description, JSON-schema input).
pub fn tools() -> Value {
    let cwd = json!({"type": "string", "description": "Only commands run in this exact folder. It must be a folder the user shared with agents; otherwise the answer is empty and a note says so."});
    json!([
        {"name": "reman_search", "description": "Find the command this user really ran for a task, before writing one yourself. Matches by meaning and by typed text; returns real commands only, never generated.\n\nHow to search:\n- One task per call, in a few plain words, the way the user would say it: \"start the dev server\", \"run database migrations\", \"deploy to vercel\". Or part of the command itself: \"docker compose up\", \"alembic upg\".\n- Name the tool or framework when you know it: \"astro dev server\", not \"dev server\". A bare \"dev server\" also matches other projects' `npm run dev` or `npx expo start`; with the name, only commands that run that tool can match.\n- Don't paste scripts, error output or long sentences; search the task, and use reman_fixes for errors.\n- If several tasks are needed, make several calls.\n\nReading the answer:\n- Only close matches come back. An EMPTY list means nothing close is known: do not fall back to a loosely similar command; look at the project's own scripts (package.json, Makefile, README) instead.\n- Read any note that follows the list: it says why the answer is empty or filtered (a folder not shared, commands that only failed left out).\n- in_this_project: ran in the project you work in. success_rate null: the outcome was never recorded. actor: who ran it (human, agent, or both).\n- Before running a result, reman_check it with the same cwd.",
         "inputSchema": {"type": "object", "properties": {"intent": {"type": "string", "description": "The task in a few plain words, or part of the command. Include the tool's name when you know it."},
                         "cwd": cwd, "worked_only": {"type": "boolean", "default": true, "description": "Leave out commands that only ever failed (default). Commands with no recorded outcome are kept."},
                         "k": {"type": "integer", "default": 5, "description": "How many results at most."},
                         "prefer": {"type": "string", "enum": ["human"], "description": "Rank commands the user ran above ones only agents ran."}}, "required": ["intent"]}},
        {"name": "reman_recent", "description": "Chronological recent REAL commands (short-term memory), optionally in one folder.",
         "inputSchema": {"type": "object", "properties": {"cwd": cwd, "n": {"type": "integer", "default": 20}}}},
        {"name": "reman_failures", "description": "Recent commands that only ever FAILED (real non-zero exit), newest first. Unknown-exit commands are not failures.",
         "inputSchema": {"type": "object", "properties": {"cwd": cwd, "n": {"type": "integer", "default": 20}}}},
        {"name": "reman_fixes", "description": "For a failed command: first the PROVEN fixes (what the user actually ran next that worked), then similar commands from their successes. Real commands only.",
         "inputSchema": {"type": "object", "properties": {"failed_command": {"type": "string"}, "cwd": cwd,
                         "error": {"type": "string", "description": "What the command printed when it failed. reman then also offers what fixed a DIFFERENT command that failed with the same error."}}, "required": ["failed_command"]}},
        {"name": "reman_check", "description": "Vet a command before running it: has this user run it, did it work, who ran it, where. Verdicts: verified | failed | mixed | ran_unknown | never_run (with nearest known commands).",
         "inputSchema": {"type": "object", "properties": {"command": {"type": "string"}, "cwd": cwd}, "required": ["command"]}},
        {"name": "reman_flows", "description": "Recurring command SEQUENCES the user runs (workflow memory), e.g. add -> commit -> push.",
         "inputSchema": {"type": "object", "properties": {"cwd": cwd, "n": {"type": "integer", "default": 15}}}},
        {"name": "reman_next", "description": "Predict the user's likely NEXT commands in a folder, from what they usually run after the last (or given) command there.",
         "inputSchema": {"type": "object", "properties": {"cwd": cwd, "last_command": {"type": "string"}, "n": {"type": "integer", "default": 5}}}},
        {"name": "reman_runbook", "description": "How this project is run, from what actually worked here: per task (set up, run, test, lint and format, build, database, deploy and release) the user's real commands with their success record (runs = worked + failed + unseen, where a pipe hid the result) and the folder each runs in (`dir`, from the project root); where the history has no whole command for a task, the one the project's files declare (`declared`: package.json, Makefile, pytest config), not run yet, plus the usual command sequences. Call it first in an unfamiliar project instead of guessing how to install, run or test it.",
         "inputSchema": {"type": "object", "properties": {"cwd": {"type": "string", "description": "The project folder (default: the folder the agent runs in). Must be inside the allowed root."}}}},
        {"name": SHARE_TOOL, "description": "Ask the user to let you see this project's command history, for this session only. Your app asks the user to approve this call: their Allow shares it, nothing is saved, and the next session asks again. Call it when a reman answer's note says your project is not shared and the history would help. If the user declines, accept it: do not call it again this session.",
         "inputSchema": {"type": "object", "properties": {"cwd": {"type": "string", "description": "A folder of the project to share (default: the project you work in)."}}}}
    ])
}

/// The tool an agent calls to have the user approve sharing its project (see Consent).
const SHARE_TOOL: &str = "reman_share_project";

/// Tool schemas in the shape each ecosystem's function calling expects.
pub fn tools_as(format: &str) -> Result<Value> {
    let t = tools();
    let list = t.as_array().cloned().unwrap_or_default();
    Ok(match format {
        "mcp" => t,
        // OpenAI Chat Completions / Agents SDK function tools
        "openai" => Value::Array(
            list.into_iter()
                .map(|x| json!({"type": "function", "function": {"name": x["name"], "description": x["description"], "parameters": x["inputSchema"]}}))
                .collect(),
        ),
        // OpenAI Responses API (flat function tools)
        "openai-responses" => Value::Array(
            list.into_iter()
                .map(|x| json!({"type": "function", "name": x["name"], "description": x["description"], "parameters": x["inputSchema"]}))
                .collect(),
        ),
        // Anthropic Messages API tools
        "anthropic" => Value::Array(
            list.into_iter().map(|x| json!({"name": x["name"], "description": x["description"], "input_schema": x["inputSchema"]})).collect(),
        ),
        other => return Err(anyhow!("unknown format {other:?} (mcp | openai | openai-responses | anthropic)")),
    })
}

/// Handle one JSON-RPC message; None for notifications (no reply).
pub fn rpc(msg: &Value, call: &mut dyn FnMut(&str, &Value) -> Result<Value>) -> Option<Value> {
    let id = msg.get("id").cloned()?;
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let reply: std::result::Result<Value, Value> = match method {
        "initialize" => Ok(json!({
            "protocolVersion": params.get("protocolVersion").and_then(Value::as_str).unwrap_or("2025-06-18"),
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "reman", "version": config::VERSION},
            "instructions": "Ground-truth memory of the commands this user really ran (with exit codes, folders, who ran them). Use it before guessing a command: reman_search the task in a few plain words, naming the tool or framework when you know it (\"astro dev server\", not \"dev server\"), reman_check a command before running it, and after a failure call reman_fixes with the failed command and its error. An empty answer means nothing is known; read the note that comes with it rather than substituting a loosely similar command. Agents see only folders the user approved; if yours is not, call reman_share_project and the app asks the user to approve it for this session - never run `reman connect` yourself."
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools()})),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            Ok(match call(name, &args).map(Answer::from_reply) {
                Ok(Answer { value: v, note }) => {
                    let mut content = vec![json!({"type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default()})];
                    let mut structured = json!({"result": v});
                    if let Some(n) = note {
                        content.push(json!({"type": "text", "text": format!("note: {n}")}));
                        structured["note"] = json!(n);
                    }
                    json!({"content": content, "structuredContent": structured, "isError": false})
                }
                Err(e) => json!({"content": [{"type": "text", "text": format!("reman error: {e}")}], "isError": true}),
            })
        }
        _ => Err(json!({"code": -32601, "message": format!("method not found: {method}")})),
    };
    Some(match reply {
        Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
        Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e}),
    })
}

/// Tool calls through the daemon (the stdio server and `reman call`).
pub struct Bridge {
    client: Option<Client>,
    pub pol: Policy,
}

impl Bridge {
    pub fn new(pol: Policy) -> Self {
        Self { client: None, pol }
    }

    pub fn call(&mut self, tool: &str, args: &Value) -> Result<Value> {
        let req = json!({"op": "mcp", "tool": tool, "args": args, "roots": self.pol.roots, "project": self.pol.project, "notes": true,
                         "allow_global": self.pol.allow_global, "strict": self.pol.strict, "old_history": self.pol.old_history});
        for attempt in 0..2 {
            if self.client.is_none() {
                self.client = Some(Client::connect()?);
            }
            match self.client.as_mut().unwrap().call(&req) {
                Ok(v) => return Ok(v),
                Err(e) if attempt == 0 && !e.to_string().starts_with("daemon:") => self.client = None, // reconnect once
                Err(e) => return Err(e),
            }
        }
        unreachable!()
    }
}

pub fn serve_stdio() -> Result<()> {
    // a thread of its own reads the app, so a question to the user can time out
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    serve(rx, std::io::stdout(), Policy::from_env())
}

/// The stdio server: JSON-RPC in, JSON-RPC out. It can also ask its app a question mid-call (a
/// request of its own), so it keeps what arrives meanwhile for later.
fn serve(input: std::sync::mpsc::Receiver<String>, output: impl Write, pol: Policy) -> Result<()> {
    let mut wire = Wire::new(input, output);
    let mut bridge = Bridge::new(pol);
    let mut consent = Consent::default();
    while let Some(msg) = wire.next() {
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        match msg.get("method").and_then(Value::as_str) {
            Some("initialize") => consent.meet(&params, std::env::var("CLAUDE_CODE_ENTRYPOINT").ok().as_deref()),
            // the agent asks to see its project: the app's approval of this call is the user's yes
            Some("tools/call") if params.get("name").and_then(Value::as_str) == Some(SHARE_TOOL) => {
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let mut ans = Some(consent.share_by_call(&mut bridge.pol, &args)?);
                if let Some(resp) = rpc(&msg, &mut |_, _| Ok(ans.take().map(Answer::envelope).unwrap_or(Value::Null))) {
                    wire.send(&resp)?;
                }
                continue;
            }
            Some("tools/call") => {
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                consent.ask_if_needed(&mut wire, &mut bridge.pol, &args)?;
            }
            // an answer to a question of ours that came too late: nothing to reply to
            None => continue,
            _ => {}
        }
        if let Some(resp) = rpc(&msg, &mut |name, args| bridge.call(name, args)) {
            wire.send(&resp)?;
        }
    }
    Ok(())
}

/// One MCP connection's wire: messages in order, plus requests of our own to the app.
struct Wire<W: Write> {
    rx: std::sync::mpsc::Receiver<String>,
    /// what the app sent while we waited for an answer, in order
    queue: std::collections::VecDeque<Value>,
    out: W,
    n: u64,
    /// how long a question to the user may take before reman carries on without an answer
    patience: std::time::Duration,
}

impl<W: Write> Wire<W> {
    fn new(rx: std::sync::mpsc::Receiver<String>, out: W) -> Self {
        Self { rx, queue: Default::default(), out, n: 0, patience: std::time::Duration::from_secs(120) }
    }

    fn parse(line: &str) -> Option<Value> {
        serde_json::from_str::<Value>(line.trim()).ok()
    }

    fn next(&mut self) -> Option<Value> {
        if let Some(m) = self.queue.pop_front() {
            return Some(m);
        }
        loop {
            if let Some(v) = Self::parse(&self.rx.recv().ok()?) {
                return Some(v);
            }
        }
    }

    fn send(&mut self, v: &Value) -> Result<()> {
        writeln!(self.out, "{}", serde_json::to_string(v)?)?;
        Ok(self.out.flush()?)
    }

    /// Ask the app (a JSON-RPC request of ours) and wait for its answer: the result, or None when
    /// it answered with an error, went away, or took longer than `patience` (an app that says it
    /// can ask and never does must not hang the agent's call).
    fn request(&mut self, method: &str, params: Value) -> Result<Option<Value>> {
        self.n += 1;
        let id = format!("reman-{}", self.n);
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))?;
        let deadline = std::time::Instant::now() + self.patience;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let Ok(line) = self.rx.recv_timeout(left) else { return Ok(None) };
            let Some(m) = Self::parse(&line) else { continue };
            if m.get("id").and_then(Value::as_str) == Some(id.as_str()) && m.get("method").is_none() {
                return Ok(m.get("result").cloned());
            }
            self.queue.push_back(m);
        }
    }
}

// the three answers, worded the same in the app's dialog and in reman's window
const SHARE_SESSION: &str = "Allow for this session";
const SHARE_ALWAYS: &str = "Always allow";
const SHARE_NO: &str = "Don't allow";

/// A folder as it is written on disk (`C:\Users\me\site`), for people: reman compares folders
/// lowercased on Windows, and a question shouldn't show them so.
fn on_disk(folder: &str) -> String {
    std::fs::canonicalize(folder)
        .map(|p| {
            let s = p.to_string_lossy().into_owned();
            s.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(s)
        })
        .unwrap_or_else(|_| folder.to_string())
}

/// An app's name as people know it, from the name it gives itself in `initialize`.
fn app_name(client: &str) -> String {
    let c = client.to_lowercase();
    let known = [
        ("claude-code", "Claude Code"),
        ("claude-ai", "Claude Desktop"),
        ("cursor", "Cursor"),
        ("windsurf", "Windsurf"),
        ("codex", "Codex"),
        ("gemini", "Gemini CLI"),
        ("opencode", "opencode"),
        ("copilot", "GitHub Copilot"),
        ("visual studio code", "VS Code"),
    ];
    known.iter().find(|(k, _)| c.contains(k)).map_or_else(|| client.to_string(), |(_, n)| n.to_string())
}

/// How to put the question to the user.
#[derive(Debug, PartialEq)]
enum Asker {
    /// the app asks (MCP elicitation)
    App,
    /// reman's own window on this desktop, for an app that can't ask
    Window,
    /// nobody can: the answer's note says how to share it instead
    Nobody,
}

/// Sharing a folder with an agent, on the user's yes. The question goes to the user, never to the
/// model: the app asks its user (MCP elicitation) and only their answer comes back, or, when the
/// app can't, reman shows its own window. So an agent still can't approve a folder for itself.
/// "For this session" lives in this process only (one agent session); "always" is saved like
/// `--add-root`.
#[derive(Default)]
struct Consent {
    /// the app can ask its user (it said so in `initialize`, and it really shows the question)
    can_ask: bool,
    /// who asks, in the app's own name ("Visual Studio Code", "claude-code")
    app: String,
    /// folders asked about this session: never twice
    asked: Vec<String>,
    /// folders the user said No to this session: an agent asking again gets No
    declined: Vec<String>,
    /// strict permissions (`reman settings`): only reman's own dialog box can share a folder
    strict: bool,
    /// reman's dialog box can be shown here (a desktop, not over SSH)
    window: bool,
}

impl Consent {
    /// `entrypoint`: $CLAUDE_CODE_ENTRYPOINT. Claude Code's VS Code extension says it can ask,
    /// then answers every question "decline" without showing it (anthropics/claude-code#79174).
    fn meet(&mut self, init: &Value, entrypoint: Option<&str>) {
        let says = init.pointer("/capabilities/elicitation").is_some_and(|c| !c.is_null());
        self.can_ask = says && entrypoint != Some("claude-vscode");
        self.app = init.pointer("/clientInfo/name").and_then(Value::as_str).map_or_else(|| "A coding agent".to_string(), app_name);
        self.strict = std::env::var("REMAN_CONSENT_WINDOW").as_deref() == Ok("on") || crate::settings::load().strict_permissions;
        self.window = window::available();
    }

    fn question(&self, folder: &str) -> String {
        format!("{} wants to use your command history from {}: the commands you and your agents ran there (secrets stay masked). Share it?", self.app, on_disk(folder))
    }

    /// The user's answer for `folder`, applied: shared for this session (this process), saved for
    /// good, or remembered as a No. Whether it is shared now.
    fn apply(&mut self, choice: &str, folder: &str, pol: &mut Policy) -> Result<bool> {
        match choice {
            SHARE_SESSION | SHARE_ALWAYS => {
                pol.roots.push(folder.to_string());
                if choice == SHARE_ALWAYS {
                    let mut st = crate::settings::load();
                    if !st.mcp_roots.iter().any(|r| config::norm_path(r) == folder) {
                        st.mcp_roots.push(on_disk(folder));
                        crate::settings::save(&st)?;
                    }
                }
                Ok(true)
            }
            SHARE_NO => {
                self.declined.push(folder.to_string());
                Ok(false)
            }
            _ => Ok(false),
        }
    }

    /// The agent called the share tool: the app asked the user to approve the call, and it got
    /// here, so the user said yes, for this session. Under strict permissions the app's approval
    /// counts for nothing: reman's own dialog box asks, and where it can't be shown, the answer is
    /// No. A folder the user said No to stays No.
    fn share_by_call(&mut self, pol: &mut Policy, args: &Value) -> Result<Answer> {
        let Some(folder) = Self::wanted(pol, args) else {
            let shared = args.get("cwd").and_then(Value::as_str).or(pol.project.as_deref()).is_some_and(|f| pol.within(Some(f)));
            return Ok(Answer::plain(if shared {
                json!({"shared": true, "note": "Already shared with you."})
            } else {
                json!({"shared": false, "reason": "There is no project folder here to share: a drive or the home folder is never shared."})
            }));
        };
        if self.declined.contains(&folder) {
            return Ok(Answer::plain(json!({"shared": false, "folder": folder, "reason": "The user said No to sharing this folder in this session. Do not ask again."})));
        }
        if self.strict && !self.window {
            return Ok(Answer::plain(json!({"shared": false, "folder": folder,
                "reason": format!("Strict permissions are on: only reman's own dialog box can share a folder, and it can't be shown here (no desktop). Ask the user to run `reman connect --add-root \"{}\"` in their own terminal.", on_disk(&folder))})));
        }
        self.asked.push(folder.clone());
        let choice = if self.strict { window::ask(&self.app, &on_disk(&folder), std::time::Duration::from_secs(120)).unwrap_or_default() } else { SHARE_SESSION.to_string() };
        Ok(Answer::plain(match self.apply(&choice, &folder, pol)? {
            true => json!({"shared": true, "folder": folder, "for": if choice == SHARE_ALWAYS { "always" } else { "this session" }}),
            false => json!({"shared": false, "folder": folder, "reason": "The user didn't share it. Do not ask again this session."}),
        }))
    }

    /// Who asks when a call needs a folder: under strict permissions only reman's dialog box
    /// (nobody, where it can't be shown); otherwise the app, when it really can ask.
    fn asker(&self) -> Asker {
        match (self.strict, self.window, self.can_ask) {
            (true, true, _) => Asker::Window,
            (true, false, _) => Asker::Nobody,
            (false, _, true) => Asker::App,
            (false, _, false) => Asker::Nobody,
        }
    }

    /// The folder a call needs that isn't shared: the one it names, else the agent's project.
    /// Never one that would be everything (a drive, the home folder).
    fn wanted(pol: &Policy, args: &Value) -> Option<String> {
        match args.get("cwd").and_then(Value::as_str).filter(|c| !c.trim().is_empty()) {
            Some(c) if !pol.within(Some(c)) => project_of(std::path::Path::new(c)),
            Some(_) => None,
            None => pol.project.clone().filter(|p| !pol.within(Some(p))),
        }
    }

    fn ask_if_needed<W: Write>(&mut self, wire: &mut Wire<W>, pol: &mut Policy, args: &Value) -> Result<()> {
        if pol.allow_global {
            return Ok(());
        }
        let asker = self.asker();
        if asker == Asker::Nobody {
            return Ok(());
        }
        let Some(folder) = Self::wanted(pol, args).filter(|f| !self.asked.contains(f)) else { return Ok(()) };
        self.asked.push(folder.clone());
        let message = self.question(&folder);
        let choice = match asker {
            Asker::App => {
                let params = json!({
                    "message": message,
                    "requestedSchema": {"type": "object", "properties": {"share": {"type": "string", "title": "Share this folder",
                                        "enum": [SHARE_SESSION, SHARE_ALWAYS, SHARE_NO], "default": SHARE_SESSION}}, "required": ["share"]}
                });
                // accept: their choice; decline: a No; cancelled or unanswered: nothing decided
                wire.request("elicitation/create", params)?.and_then(|a| match a.get("action").and_then(Value::as_str) {
                    Some("accept") => a.pointer("/content/share").and_then(Value::as_str).map(String::from),
                    Some("decline") => Some(SHARE_NO.to_string()),
                    _ => None,
                })
            }
            Asker::Window => window::ask(&self.app, &on_disk(&folder), wire.patience),
            Asker::Nobody => None,
        };
        self.apply(&choice.unwrap_or_default(), &folder, pol)?;
        Ok(())
    }
}

/// reman's own question window, for an app that can't ask its user: shown on this desktop by the
/// `reman mcp` process the app started, answered by a click. None when nobody answered in time.
mod window {
    use super::{SHARE_ALWAYS, SHARE_NO, SHARE_SESSION};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// Is there a desktop to show it on? Not over SSH, not on a Linux without a display or
    /// zenity / kdialog;
    /// `REMAN_CONSENT_WINDOW=off` turns it off.
    pub fn available() -> bool {
        if std::env::var("REMAN_CONSENT_WINDOW").as_deref() == Ok("off") || std::env::var_os("SSH_CONNECTION").is_some() {
            return false;
        }
        if cfg!(windows) || cfg!(target_os = "macos") {
            return true;
        }
        let display = ["DISPLAY", "WAYLAND_DISPLAY"].iter().any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()));
        let has = |tool: &str| std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(tool).is_file()));
        display && (has("zenity") || has("kdialog"))
    }

    /// Ask whether `app` may see `folder`, in a window of reman's own; the choice clicked.
    pub fn ask(app: &str, folder: &str, patience: Duration) -> Option<String> {
        let mut cmd = command(app, folder);
        let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().ok()?;
        let deadline = Instant::now() + patience;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(100)),
                _ => {
                    let _ = child.kill();
                    return None;
                }
            }
        }
        let mut out = String::new();
        std::io::Read::read_to_string(&mut child.stdout.take()?, &mut out).ok()?;
        chosen(&out)
    }

    /// What the window printed, as one of the choices.
    fn chosen(out: &str) -> Option<String> {
        let out = out.trim();
        [SHARE_SESSION, SHARE_ALWAYS, SHARE_NO].iter().find(|c| out.ends_with(*c)).map(|c| c.to_string())
    }

    /// Windows: a WPF window (init/consent.ps1), crisp at any scaling, in light or dark mode.
    #[cfg(windows)]
    fn command(app: &str, folder: &str) -> Command {
        let q = |s: &str| s.replace('\'', "''"); // inside the script's '...' strings
        let script = include_str!("init/consent.ps1")
            .replace("@@APP@@", &q(app))
            .replace("@@FOLDER@@", &q(folder))
            .replace("@@SESSION@@", &q(SHARE_SESSION))
            .replace("@@ALWAYS@@", &q(SHARE_ALWAYS))
            .replace("@@NO@@", &q(SHARE_NO))
            // REMAN_CONSENT_RENDER=<png>: draw it into that image instead of showing it (a look at
            // it, for docs and checks); no click, so no answer, so nothing is shared
            .replace("@@RENDER@@", &q(&std::env::var("REMAN_CONSENT_RENDER").unwrap_or_default()));
        let encoded: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let mut cmd = Command::new("powershell");
        cmd.args(["-NoProfile", "-NonInteractive", "-STA", "-WindowStyle", "Hidden", "-EncodedCommand", &base64(&encoded)]);
        cmd
    }

    /// macOS: a native alert (init/consent-macos.js, NSAlert through JavaScript for Automation).
    #[cfg(target_os = "macos")]
    fn command(app: &str, folder: &str) -> Command {
        let mut cmd = Command::new("osascript");
        cmd.args(["-l", "JavaScript", "-e", include_str!("init/consent-macos.js"), app, folder, SHARE_SESSION, SHARE_ALWAYS, SHARE_NO]);
        cmd
    }

    /// Linux: zenity or kdialog (init/consent-linux.sh), in the desktop's own theme.
    #[cfg(all(unix, not(target_os = "macos")))]
    fn command(app: &str, folder: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", include_str!("init/consent-linux.sh"), "sh", app, folder, SHARE_SESSION, SHARE_ALWAYS, SHARE_NO]);
        cmd
    }

    #[cfg(windows)]
    fn base64(b: &[u8]) -> String {
        const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut s = String::with_capacity(b.len().div_ceil(3) * 4);
        for c in b.chunks(3) {
            let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
            for i in 0..4 {
                s.push(if i <= c.len() { T[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
            }
        }
        s
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn what_the_window_says() {
            assert_eq!(super::chosen(super::SHARE_SESSION).as_deref(), Some(super::SHARE_SESSION));
            assert_eq!(super::chosen(&format!("{}\n", super::SHARE_ALWAYS)).as_deref(), Some(super::SHARE_ALWAYS));
            assert_eq!(super::chosen(super::SHARE_NO).as_deref(), Some(super::SHARE_NO));
            assert_eq!(super::chosen("").as_deref(), None);
        }

        #[test]
        fn the_scripts_take_every_value() {
            for s in [include_str!("init/consent.ps1"), include_str!("init/consent-macos.js"), include_str!("init/consent-linux.sh")] {
                assert!(s.contains("Share your command history?"));
            }
            let ps = include_str!("init/consent.ps1");
            for p in ["@@APP@@", "@@FOLDER@@", "@@SESSION@@", "@@ALWAYS@@", "@@NO@@", "@@RENDER@@"] {
                assert!(ps.contains(&format!("'{p}'")), "{p} sits in a '...' string");
            }
        }

        #[cfg(windows)]
        #[test]
        fn base64_as_powershell_reads_it() {
            assert_eq!(super::base64(b"Man"), "TWFu");
            assert_eq!(super::base64(b"Ma"), "TWE=");
            assert_eq!(super::base64(b"M"), "TQ==");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_project_is_the_repo_and_a_worktree_belongs_to_its_repo() {
        let tmp = std::env::temp_dir().join(format!("reman-project-{}", std::process::id()));
        let repo = tmp.join("site");
        let wt = repo.join(".claude").join("worktrees").join("x");
        std::fs::create_dir_all(repo.join(".git").join("worktrees").join("x")).unwrap();
        std::fs::create_dir_all(wt.join("src")).unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}/.git/worktrees/x\n", repo.to_string_lossy().replace('\\', "/"))).unwrap();
        let want = Some(config::norm_path(&repo.to_string_lossy()));
        assert_eq!(project_of(&repo.join("src")), want, "a folder inside the repo");
        assert_eq!(project_of(&wt.join("src")), want, "a worktree of the repo");
        let plain = tmp.join("notes");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(project_of(&plain), Some(config::norm_path(&plain.to_string_lossy())), "no repo: the folder itself");
        if let Some(h) = dirs::home_dir() {
            assert_eq!(project_of(&h), None, "never the whole home folder");
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// A wire the app "sends" `lines` on; the sender is kept open (the app still there).
    fn wire_of<'a>(lines: &[Value], out: &'a mut Vec<u8>) -> (Wire<&'a mut Vec<u8>>, std::sync::mpsc::Sender<String>) {
        let (tx, rx) = std::sync::mpsc::channel();
        for l in lines {
            tx.send(l.to_string()).unwrap();
        }
        (Wire::new(rx, out), tx)
    }

    fn site() -> (&'static str, Policy) {
        let project = if cfg!(windows) { r"D:\site" } else { "/home/u/site" };
        (project, Policy { roots: vec![], allow_global: false, strict: false, old_history: false, project: Some(config::norm_path(project)) })
    }

    #[test]
    fn the_user_shares_a_folder_for_this_session() {
        let (project, mut pol) = site();
        let mut consent = Consent::default();
        consent.meet(&json!({"capabilities": {"elicitation": {}}, "clientInfo": {"name": "Visual Studio Code"}}), None);
        // the app's answer arrives after an unrelated notification, which is kept for later
        let mut out = Vec::new();
        let (mut wire, _app) = wire_of(
            &[
                json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {}}),
                json!({"jsonrpc": "2.0", "id": "reman-1", "result": {"action": "accept", "content": {"share": SHARE_SESSION}}}),
            ],
            &mut out,
        );
        consent.ask_if_needed(&mut wire, &mut pol, &json!({"intent": "run the tests"})).unwrap();
        assert!(pol.within(Some(project)), "shared for this session");
        assert_eq!(wire.queue.len(), 1, "the notification waits its turn");
        // asked once: a second call doesn't ask again
        consent.ask_if_needed(&mut wire, &mut pol, &json!({})).unwrap();
        drop(wire);
        let asked = String::from_utf8(out).unwrap();
        assert_eq!(asked.matches("elicitation/create").count(), 1);
        assert!(asked.contains("VS Code wants to use your command history"));
    }

    #[test]
    fn no_means_no() {
        let (project, mut pol) = site();
        let mut consent = Consent::default();
        consent.meet(&json!({"capabilities": {"elicitation": {}}}), None);
        let mut out = Vec::new();
        let (mut wire, _app) = wire_of(&[json!({"jsonrpc": "2.0", "id": "reman-1", "result": {"action": "accept", "content": {"share": SHARE_NO}}})], &mut out);
        consent.ask_if_needed(&mut wire, &mut pol, &json!({})).unwrap();
        consent.ask_if_needed(&mut wire, &mut pol, &json!({})).unwrap();
        drop(wire);
        assert!(!pol.within(Some(project)));
        assert_eq!(String::from_utf8(out).unwrap().matches("elicitation/create").count(), 1, "not asked again");
    }

    #[test]
    fn apps_by_the_names_people_know() {
        assert_eq!(app_name("claude-code"), "Claude Code");
        assert_eq!(app_name("Visual Studio Code"), "VS Code");
        assert_eq!(app_name("cursor-vscode"), "Cursor");
        assert_eq!(app_name("gemini-cli-mcp-client"), "Gemini CLI");
        assert_eq!(app_name("Some New App"), "Some New App");
    }

    #[test]
    fn who_asks() {
        let mut c = Consent::default();
        let asks = |c: &mut Consent, caps: Value, entry: Option<&str>, strict: bool, window: bool| {
            c.meet(&json!({"capabilities": caps}), entry);
            (c.strict, c.window) = (strict, window);
            c.asker()
        };
        // normally the app asks, when it really can
        assert_eq!(asks(&mut c, json!({"elicitation": {}}), Some("cli"), false, true), Asker::App);
        // Claude Code's VS Code extension says it can ask, then declines unseen: its Allow buttons
        // on reman_share_project decide instead
        assert_eq!(asks(&mut c, json!({"elicitation": {}}), Some("claude-vscode"), false, true), Asker::Nobody);
        assert_eq!(asks(&mut c, json!({}), None, false, true), Asker::Nobody);
        // strict permissions: only reman's dialog box, even where the app could ask; none at all
        // where it can't be shown
        assert_eq!(asks(&mut c, json!({"elicitation": {}}), Some("cli"), true, true), Asker::Window);
        assert_eq!(asks(&mut c, json!({"elicitation": {}}), Some("cli"), true, false), Asker::Nobody);
    }

    #[test]
    fn strict_permissions_no_dialog_no_share() {
        let (project, mut pol) = site();
        let mut c = Consent::default();
        c.meet(&json!({"capabilities": {"elicitation": {}}}), None);
        (c.strict, c.window) = (true, false);
        // the app approved the call, but under strict permissions that counts for nothing
        let a = c.share_by_call(&mut pol, &json!({})).unwrap();
        assert_eq!(a.value["shared"], json!(false));
        assert!(a.value["reason"].as_str().unwrap().contains("Strict permissions"));
        assert!(!pol.within(Some(project)));
        // and nothing is asked through the app either
        let mut out = Vec::new();
        let (mut wire, _app) = wire_of(&[], &mut out);
        c.ask_if_needed(&mut wire, &mut pol, &json!({})).unwrap();
        drop(wire);
        assert!(out.is_empty());
    }

    #[test]
    fn the_agent_asks_and_the_apps_approval_is_the_yes() {
        let (project, mut pol) = site();
        let mut c = Consent::default();
        c.meet(&json!({"capabilities": {"elicitation": {}}}), Some("claude-vscode"));
        c.window = false;
        let a = c.share_by_call(&mut pol, &json!({})).unwrap();
        assert_eq!((a.value["shared"].clone(), a.value["for"].clone()), (json!(true), json!("this session")));
        assert!(pol.within(Some(project)));
        assert_eq!(c.share_by_call(&mut pol, &json!({})).unwrap().value["shared"], json!(true), "already shared");
    }

    #[test]
    fn after_a_no_the_share_tool_gets_no() {
        let (project, mut pol) = site();
        let mut c = Consent::default();
        c.meet(&json!({"capabilities": {"elicitation": {}}}), None);
        c.window = false;
        let mut out = Vec::new();
        let (mut wire, _app) = wire_of(&[json!({"jsonrpc": "2.0", "id": "reman-1", "result": {"action": "decline"}})], &mut out);
        c.ask_if_needed(&mut wire, &mut pol, &json!({})).unwrap();
        let a = c.share_by_call(&mut pol, &json!({})).unwrap();
        assert_eq!(a.value["shared"], json!(false));
        assert!(!pol.within(Some(project)));
    }

    #[test]
    fn an_app_that_never_answers_doesnt_hang_the_call() {
        let (project, mut pol) = site();
        let mut consent = Consent::default();
        consent.meet(&json!({"capabilities": {"elicitation": {}}}), None);
        let mut out = Vec::new();
        let (mut wire, _app) = wire_of(&[], &mut out);
        wire.patience = std::time::Duration::from_millis(80);
        let t = std::time::Instant::now();
        consent.ask_if_needed(&mut wire, &mut pol, &json!({})).unwrap();
        assert!(t.elapsed() < std::time::Duration::from_secs(2) && !pol.within(Some(project)));
    }

    #[test]
    fn nobody_to_ask_nothing_asked() {
        let (project, mut pol) = site();
        let mut consent = Consent::default();
        consent.meet(&json!({"capabilities": {}}), None);
        consent.window = false;
        let mut out = Vec::new();
        let (mut wire, _app) = wire_of(&[], &mut out);
        consent.ask_if_needed(&mut wire, &mut pol, &json!({})).unwrap();
        drop(wire);
        assert!(out.is_empty() && !pol.within(Some(project)));
    }

    #[test]
    fn a_note_survives_the_trip_and_bare_results_still_read() {
        let a = Answer::from_reply(Answer { value: json!([]), note: Some("why".into()) }.envelope());
        assert_eq!((a.value, a.note.as_deref()), (json!([]), Some("why")));
        let a = Answer::from_reply(json!([{"command": "git status"}]));
        assert_eq!((a.value[0]["command"].as_str(), a.note), (Some("git status"), None));
        let a = Answer::from_reply(json!({"command": "x", "verdict": "verified"}));
        assert_eq!(a.value["verdict"], "verified");
    }

    #[test]
    fn boundary() {
        // Windows paths compare case-insensitively; Unix paths don't
        let (root, sub, evil, other) = if cfg!(windows) {
            (r"D:\Work", r"d:\work\api", r"D:\WorkEvil", r"C:\Users")
        } else {
            ("/home/u/Work", "/home/u/Work/api", "/home/u/WorkEvil", "/home/u")
        };
        let p = Policy { roots: vec![config::norm_path(root)], allow_global: false, strict: false, old_history: false, project: None };
        assert!(p.within(Some(root)));
        assert!(p.within(Some(sub)));
        assert!(!p.within(Some(evil)));
        assert!(!p.within(Some(other)));
        assert!(!p.within(None));
    }

    #[test]
    fn old_history_is_opt_in_and_generic_only() {
        use crate::db::Run;
        let mk = |cmd: &str, cwd: Option<&str>| Run { cmd: cmd.into(), exit: None, cwd: cwd.map(Into::into), session: "s".into(), actor: "human".into(), ts: 1, duration_ms: None, ..Default::default() };
        let mut st = Store::default();
        st.apply_run(1, true, &mk("docker ps", None), None);
        st.apply_run(2, true, &mk("docker ps", Some("unknown")), None);
        st.apply_run(3, true, &mk("python train.py", None), None);
        st.apply_run(4, true, &mk("docker ps", Some(r"C:\elsewhere")), None);
        let off = Policy { roots: vec![config::norm_path(r"D:\P")], allow_global: false, strict: false, old_history: false, project: None };
        let on = Policy { old_history: true, ..off.clone() };
        let e = |t: &str| st.entry(t).unwrap().1;
        assert!(visible(&st, e("docker ps"), &off, None).is_empty());
        assert_eq!(visible(&st, e("docker ps"), &on, None).len(), 2); // never the C:\elsewhere row
        assert!(visible(&st, e("docker ps"), &on, Some(r"D:\P")).is_empty()); // a folder was asked for
        assert!(visible(&st, e("python train.py"), &on, None).is_empty()); // names a project file
        assert_eq!(sum(&st, &visible(&st, e("docker ps"), &on, None)).cwd, None);
    }
}

#[cfg(test)]
mod rpc_tests {
    use super::*;

    #[test]
    fn jsonrpc_and_formats() {
        let mut call = |name: &str, _a: &Value| -> Result<Value> { Ok(json!([{"command": name}])) };
        let init = rpc(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}}), &mut call).unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert!(rpc(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}), &mut call).is_none());
        let list = rpc(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}), &mut call).unwrap();
        assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 9);
        let res = rpc(&json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "reman_next", "arguments": {}}}), &mut call).unwrap();
        assert_eq!(res["result"]["structuredContent"]["result"][0]["command"], "reman_next");
        assert_eq!(rpc(&json!({"jsonrpc": "2.0", "id": 4, "method": "nope"}), &mut call).unwrap()["error"]["code"], -32601);
        assert_eq!(tools_as("openai").unwrap()[0]["function"]["name"], "reman_search");
        assert!(tools_as("anthropic").unwrap()[0]["input_schema"].is_object());
        assert_eq!(tools_as("openai-responses").unwrap()[0]["name"], "reman_search");
    }
}
