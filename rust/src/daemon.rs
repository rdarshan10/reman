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
}

pub fn log(msg: &str) {
    let line = format!("[{}] {msg}\n", config::now());
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(config::log_path()) {
        let _ = f.write_all(line.as_bytes());
    }
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
        exit: v.get("exit").and_then(Value::as_i64).filter(|e| *e >= 0),
        cwd: s(v, "cwd").map(str::to_string),
        session: s(v, "session").unwrap_or("").to_string(),
        actor: s(v, "actor").unwrap_or(default_actor).to_string(),
        ts: v.get("ts").and_then(Value::as_i64).filter(|t| *t > 0).unwrap_or_else(config::now),
        duration_ms: v.get("duration_ms").and_then(Value::as_i64).filter(|d| *d >= 0),
    })
}

/// blank, control-char-only (the \x07 beep) or 1-char noise
pub fn is_junk(cmd: &str) -> bool {
    cmd.chars().filter(|c| !c.is_control()).collect::<String>().trim().chars().count() < 2
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
        Ok(Self { store: RwLock::new(store), fixes: Mutex::new(fixes), db: Mutex::new(conn), embedder, started: Instant::now() })
    }

    /// Write path for everything (hooks, spool, imports). Embeds new texts outside any lock,
    /// then one db transaction, then mirrors into memory. Returns (#new texts, per-run suggestion).
    pub fn ingest_runs(&self, runs: &[Run]) -> Result<(usize, Vec<Option<Value>>)> {
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
        let suggestions = runs.iter().map(|r| if r.failed() { self.suggest(&r.cmd, r.cwd.as_deref()) } else { None }).collect();
        Ok((new_texts.len(), suggestions))
    }

    /// One-line hint after a failure: a proven fix, else a strong typo match that worked.
    pub fn suggest(&self, failed: &str, cwd: Option<&str>) -> Option<Value> {
        let st = self.store.read();
        if let Some(p) = self.fixes.lock().lookup(failed, &st).into_iter().next() {
            return Some(json!({"command": p.fixed, "kind": "proven", "confidence": p.confidence, "times": p.count}));
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
            "intent": e.gkey, "description": e.desc,
            "cwd": a.cwd.map(|c| st.cwd_name(c)),
            "folders": a_all.folders, "pinned": e.pinned,
        });
        if h.sim > -1.0 {
            v["similarity"] = json!((h.sim * 1000.0).round() / 1000.0);
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

    fn search_op(&self, req: &Value, browse: bool) -> Result<Value> {
        let text = if browse { "" } else { s(req, "query").unwrap_or("") };
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
        };
        let out = search::search(&st, &q, qv.as_deref().map(|v| v.as_slice()));
        let t_rank = t0.elapsed() - t_embed;
        let results: Vec<Value> = out.hits.iter().map(|h| self.item(&st, h, scope)).collect();
        if std::env::var_os("REMAN_TRACE").is_some() {
            log(&format!("search {text:?}: embed {t_embed:?} rank {t_rank:?} total {:?}", t0.elapsed()));
        }
        Ok(json!({"mode": out.mode, "confident": out.confident, "total": out.total, "results": results}))
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
            let mut v = self.item(&st, &search::Hit { idx: i, score: 1.0, sim: -2.0, fuzzy: 0.0, variants: 1, matched_terms: 0 }, Scope::All);
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
            let mut v = self.item(&st, &search::Hit { idx: sg.idx, score: sg.score, sim: -2.0, fuzzy: 0.0, variants: 1, matched_terms: 0 }, Scope::All);
            v["similarity"] = json!((sg.score * 1000.0).round() / 1000.0);
            v["typo"] = json!((sg.typo * 1000.0).round() / 1000.0);
            v["intent_sim"] = json!((sg.sem * 1000.0).round() / 1000.0);
            results.push(v);
        }
        Ok(json!({"results": results}))
    }

    fn next_op(&self, req: &Value) -> Value {
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
                let mut v = self.item(&st, &search::Hit { idx: p.idx, score: p.score, sim: -2.0, fuzzy: 0.0, variants: 1, matched_terms: 0 }, Scope::All);
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
        self.reload()?;
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
                              "pid": std::process::id(), "engine": "rust"}),
            "ingest" => match run_from(req, "agent:claude-code") {
                None => json!({"ok": false, "skipped": "empty"}),
                Some(r) => {
                    let (new, sug) = self.ingest_runs(std::slice::from_ref(&r))?;
                    let mut v = json!({"ok": true, "new": new > 0});
                    if let Some(Some(sg)) = sug.into_iter().next() {
                        v["suggest"] = sg;
                    }
                    v
                }
            },
            "ingest_batch" => {
                let runs: Vec<Run> = req.get("items").and_then(Value::as_array).map(|a| a.iter().filter_map(|v| run_from(v, "human")).collect()).unwrap_or_default();
                let (new, _) = self.ingest_runs(&runs)?;
                json!({"ok": true, "ingested": runs.len(), "new": new})
            }
            "search" => self.search_op(req, false)?,
            "recent" => self.search_op(req, true)?,
            "didyoumean" => self.dym_op(req)?,
            "fixfor" => json!({"suggest": self.suggest(s(req, "command").unwrap_or(""), s(req, "cwd"))}),
            "next" => self.next_op(req),
            "flows" => self.flows_op(req),
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
                        let mut v = self.item(&st, &search::Hit { idx: i, score: 0.0, sim: -2.0, fuzzy: 0.0, variants: 1, matched_terms: 0 }, Scope::All);
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
            "mcp" => mcp::call_tool(self, s(req, "tool").unwrap_or(""), req.get("args").unwrap_or(&Value::Null), &mcp::Policy::from_req(req))?,
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
        std::thread::spawn(move || {
            let mut tick = 0u64;
            loop {
                std::thread::sleep(std::time::Duration::from_secs(15));
                tick += 1;
                if let Err(e) = d.drain_spool() {
                    log(&format!("spool drain failed: {e}"));
                }
                if tick % 240 == 0 {
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
