//! MCP agent interface. Two halves:
//!  - `call_tool` runs INSIDE the daemon (warm model, in-memory history), enforcing the security
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
}

impl Policy {
    /// Server config comes from the MCP process env - never from tool arguments (an agent can pass
    /// any cwd, so cwd is untrusted input).
    pub fn from_env() -> Self {
        let sep = if cfg!(windows) { ';' } else { ':' };
        let raw = std::env::var("REMAN_MCP_ROOT")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default());
        Self {
            roots: raw.split(sep).filter(|p| !p.trim().is_empty()).map(config::norm_path).collect(),
            allow_global: std::env::var("REMAN_MCP_ALLOW_GLOBAL").as_deref() == Ok("1"),
            strict: std::env::var("REMAN_MCP_STRICT_SECRETS").as_deref() == Ok("1"),
        }
    }

    pub fn from_req(req: &Value) -> Self {
        Self {
            roots: req.get("roots").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(config::norm_path).collect()).unwrap_or_default(),
            allow_global: req.get("allow_global").and_then(Value::as_bool).unwrap_or(false),
            strict: req.get("strict").and_then(Value::as_bool).unwrap_or(false),
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

    fn safe(&self, cmd: &str) -> Option<String> {
        redact::safe_command(cmd, self.strict)
    }
}

fn folder_eq(a: Option<&str>, b: &str) -> bool {
    a.is_some_and(|a| config::norm_path(a) == config::norm_path(b))
}

/// Rows of an entry the agent may see (inside the root, and in `cwd` when given), newest first.
fn visible<'a>(st: &Store, e: &'a Entry, pol: &Policy, cwd: Option<&str>) -> Vec<&'a Row> {
    let mut rows: Vec<&Row> = e
        .rows
        .iter()
        .filter(|r| {
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
        cwd: rows.first().and_then(|r| r.cwd).map(|c| st.cwd_name(c).to_string()),
    }
}

fn arg_s<'a>(a: &'a Value, k: &str) -> Option<&'a str> {
    a.get(k).and_then(Value::as_str).filter(|s| !s.is_empty())
}

fn arg_n(a: &Value, k: &str, d: i64) -> usize {
    a.get(k).and_then(Value::as_i64).unwrap_or(d).clamp(1, 500) as usize
}

pub fn call_tool(d: &Daemon, name: &str, args: &Value, pol: &Policy) -> Result<Value> {
    let cwd = arg_s(args, "cwd");
    if cwd.is_some() && !pol.within(cwd) {
        return Ok(if name == "reman_check" {
            json!({"command": arg_s(args, "command").unwrap_or(""), "verdict": "denied",
                   "advice": "That folder is outside the allowed root for this agent.", "generated": false})
        } else {
            json!([])
        });
    }
    match name {
        "reman_search" => tool_search(d, args, pol, cwd),
        "reman_recent" => Ok(tool_recent(d, args, pol, cwd, false)),
        "reman_failures" => Ok(tool_recent(d, args, pol, cwd, true)),
        "reman_fixes" => tool_fixes(d, arg_s(args, "failed_command").unwrap_or(""), pol, cwd, 3),
        "reman_check" => tool_check(d, arg_s(args, "command").unwrap_or(""), pol, cwd),
        "reman_flows" => Ok(tool_flows(d, args, pol)),
        "reman_next" => Ok(tool_next(d, args, pol, cwd)),
        other => Err(anyhow!("unknown tool {other}")),
    }
}

fn tool_search(d: &Daemon, a: &Value, pol: &Policy, cwd: Option<&str>) -> Result<Value> {
    let intent = arg_s(a, "intent").unwrap_or("");
    let k = arg_n(a, "k", 5);
    let worked = a.get("worked_only").and_then(Value::as_bool).unwrap_or(true);
    let prefer_human = arg_s(a, "prefer") == Some("human");
    let qv = if intent.trim().chars().count() >= 3 { Some(d.embedder.embed_query(intent.trim())?) } else { None };
    let st = d.store.read();
    let q = Query { text: intent, k: 400, offset: 0, scope: Scope::All, actor: None, status: worked.then_some("ok"), group: true, rank: Rank::Hybrid, here: cwd.and_then(|c| st.cwd_index(c)) };
    let out = search::search(&st, &q, qv.as_deref().map(|v| v.as_slice()));
    let mut res: Vec<(Value, u32)> = Vec::new();
    for h in &out.hits {
        let e = &st.entries[h.idx as usize];
        let rows = visible(&st, e, pol, cwd);
        if rows.is_empty() {
            continue;
        }
        let Some(safe) = pol.safe(&e.text) else { continue };
        let s = sum(&st, &rows);
        let v = if out.mode == "manual" {
            json!({"command": safe, "run_count": s.runs, "match": "literal", "generated": false})
        } else {
            let mut v = json!({"command": safe, "cwd": s.cwd, "run_count": s.runs, "intent": e.gkey,
                "variants": h.variants, "similarity": (h.sim.max(0.0) * 1000.0).round() / 1000.0,
                "description": e.desc, "generated": false,
                "success_rate": (s.ok + s.fail > 0).then(|| ((s.ok as f64 / (s.ok + s.fail) as f64) * 100.0).round() / 100.0),
                "last_run": config::age(s.last_used), "actor": s.actors.first().cloned().unwrap_or_else(|| "human".into())});
            if out.mode == "fuzzy" {
                v["match"] = json!("fuzzy");
            }
            if prefer_human {
                v["human_runs"] = json!(s.human);
                v["agent_runs"] = json!(s.agent);
            }
            v
        };
        res.push((v, s.human));
        if res.len() >= k * 3 {
            break;
        }
    }
    if prefer_human {
        // human-verified first, semantic order preserved within each bucket
        res.sort_by_key(|(_, h)| if *h > 0 { 0 } else { 1 });
    }
    Ok(Value::Array(res.into_iter().take(k).map(|x| x.0).collect()))
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
        out.push(json!({"fixed_command": safe, "confidence": 1.0, "proven": true, "fix_type": p.confidence,
                        "times_fixed": p.count, "generated": false}));
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
        out.push(json!({"fixed_command": safe, "confidence": (sg.score * 1000.0).round() / 1000.0,
                        "typo_sim": (sg.typo * 1000.0).round() / 1000.0, "intent_sim": (sg.sem * 1000.0).round() / 1000.0,
                        "proven": false, "generated": false}));
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
    if verdict == "failed" || verdict == "mixed" {
        let fixes = tool_fixes(d, cmd, pol, cwd, 1)?;
        if let Some(f) = fixes.as_array().and_then(|a| a.first()).filter(|f| f["proven"] == json!(true)) {
            out["known_fix"] = f["fixed_command"].clone();
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
// stdio JSON-RPC bridge
// ---------------------------------------------------------------------------------------------

fn tools() -> Value {
    let cwd = json!({"type": "string", "description": "Only commands run in this exact folder (must be inside the allowed root)."});
    json!([
        {"name": "reman_search", "description": "Retrieve the user's REAL past commands by meaning (semantic + fuzzy). worked_only restricts to commands seen to exit 0; prefer='human' ranks human-verified commands above agent-run ones. Returns real commands only - never generated. Prefer these over writing a command from scratch.",
         "inputSchema": {"type": "object", "properties": {"intent": {"type": "string"}, "cwd": cwd, "worked_only": {"type": "boolean", "default": true},
                         "k": {"type": "integer", "default": 5}, "prefer": {"type": "string", "enum": ["human"]}}, "required": ["intent"]}},
        {"name": "reman_recent", "description": "Chronological recent REAL commands (short-term memory), optionally in one folder.",
         "inputSchema": {"type": "object", "properties": {"cwd": cwd, "n": {"type": "integer", "default": 20}}}},
        {"name": "reman_failures", "description": "Recent commands that only ever FAILED (real non-zero exit), newest first. Unknown-exit commands are not failures.",
         "inputSchema": {"type": "object", "properties": {"cwd": cwd, "n": {"type": "integer", "default": 20}}}},
        {"name": "reman_fixes", "description": "For a failed command: first the PROVEN fixes (what the user actually ran next that worked), then similar commands from their successes. Real commands only.",
         "inputSchema": {"type": "object", "properties": {"failed_command": {"type": "string"}, "cwd": cwd}, "required": ["failed_command"]}},
        {"name": "reman_check", "description": "Vet a command before running it: has this user run it, did it work, who ran it, where. Verdicts: verified | failed | mixed | ran_unknown | never_run (with nearest known commands).",
         "inputSchema": {"type": "object", "properties": {"command": {"type": "string"}, "cwd": cwd}, "required": ["command"]}},
        {"name": "reman_flows", "description": "Recurring command SEQUENCES the user runs (workflow memory), e.g. add -> commit -> push.",
         "inputSchema": {"type": "object", "properties": {"cwd": cwd, "n": {"type": "integer", "default": 15}}}},
        {"name": "reman_next", "description": "Predict the user's likely NEXT commands in a folder, from what they usually run after the last (or given) command there.",
         "inputSchema": {"type": "object", "properties": {"cwd": cwd, "last_command": {"type": "string"}, "n": {"type": "integer", "default": 5}}}}
    ])
}

struct Bridge {
    client: Option<Client>,
    pol: Policy,
}

impl Bridge {
    fn call(&mut self, tool: &str, args: &Value) -> Result<Value> {
        let req = json!({"op": "mcp", "tool": tool, "args": args, "roots": self.pol.roots,
                         "allow_global": self.pol.allow_global, "strict": self.pol.strict});
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
    let mut bridge = Bridge { client: None, pol: Policy::from_env() };
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let Some(id) = msg.get("id").cloned() else { continue }; // notifications need no reply
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let reply = match method {
            "initialize" => Ok(json!({
                "protocolVersion": params.get("protocolVersion").and_then(Value::as_str).unwrap_or("2025-06-18"),
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "reman", "version": config::VERSION},
                "instructions": "Ground-truth memory of the commands this user really ran (with exit codes, folders, who ran them). Prefer reman_search / reman_check / reman_fixes results over guessing a command."
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tools()})),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                Ok(match bridge.call(name, &args) {
                    Ok(v) => json!({"content": [{"type": "text", "text": serde_json::to_string_pretty(&v)?}],
                                    "structuredContent": {"result": v}, "isError": false}),
                    Err(e) => json!({"content": [{"type": "text", "text": format!("reman error: {e}")}], "isError": true}),
                })
            }
            _ => Err(json!({"code": -32601, "message": format!("method not found: {method}")})),
        };
        let resp = match reply {
            Ok(r) => json!({"jsonrpc": "2.0", "id": id, "result": r}),
            Err(e) => json!({"jsonrpc": "2.0", "id": id, "error": e}),
        };
        writeln!(stdout, "{}", serde_json::to_string(&resp)?)?;
        stdout.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundary() {
        let p = Policy { roots: vec![config::norm_path(r"D:\PlanetNaidu")], allow_global: false, strict: false };
        assert!(p.within(Some(r"D:\PlanetNaidu")));
        assert!(p.within(Some(r"d:\planetnaidu\api")));
        assert!(!p.within(Some(r"D:\PlanetNaiduEvil")));
        assert!(!p.within(Some(r"C:\Users")));
        assert!(!p.within(None));
    }
}
