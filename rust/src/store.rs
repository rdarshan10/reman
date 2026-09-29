//! In-memory model of the whole history. Everything the hot path needs (vectors, provenance,
//! per-folder stats, the execution log) lives here, so search / browse / did-you-mean never touch
//! SQLite. SQLite is the durable copy; the daemon mirrors every write into this struct.
use crate::config::{self, DIM};
use crate::db::{self, Run};
use crate::describe;
use anyhow::Result;
use rusqlite::Connection;
use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct Row {
    pub id: i64,
    pub cwd: Option<u32>,
    pub runs: u32,
    pub ok: u32,
    pub fail: u32,
    pub human: u32,
    pub agent: u32,
    pub last_used: i64,
    pub last_exit: Option<i64>,
    pub last_actor: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub text: String,
    pub lower: String,
    /// bitmask of the (lowercase ASCII) characters in the text - fuzzy prefilter
    pub mask: u64,
    pub gkey: String,
    /// variants that differ only in data (messages, paths, ids) share a shape; the finder folds those
    pub shape: String,
    pub desc: Option<String>,
    pub rows: Vec<Row>,
    pub pinned: bool,
    pub comment: bool,
    /// reman's own invocations (`reman search ...`, the legacy reman_*.py scripts): recorded,
    /// but kept out of recall unless the query itself is about reman
    pub self_ref: bool,
    pub alive: bool,
    pub has_vec: bool,
    /// pasted source code that PSReadLine recorded as if it were a command (no folder, never an
    /// outcome, reads like Python/JS). Kept in the db, never offered back.
    pub noise: bool,
}

impl Entry {
    /// Should this command be offered back to the user / an agent?
    pub fn recallable(&self, show_self: bool) -> bool {
        self.alive && !self.comment && !self.noise && (show_self || !self.self_ref)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Exec {
    pub ts: i64,
    pub entry: u32,
    pub row_id: i64,
    pub cwd: Option<u32>,
    pub session: u32,
    pub exit: Option<i64>,
}

/// Which rows of an entry count: all of them, one folder, or every folder of one repo.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Scope {
    All,
    Folder(u32),
    Repo(u32),
    /// requested folder/repo that has never been seen -> matches nothing
    Nothing,
}

#[derive(Debug, Clone, Default)]
pub struct Agg {
    pub runs: u32,
    pub ok: u32,
    pub fail: u32,
    pub human: u32,
    pub agent: u32,
    pub last_used: i64,
    pub last_exit: Option<i64>,
    pub last_actor: Option<String>,
    pub cwd: Option<u32>,
    pub folders: u32,
}

impl Agg {
    pub fn status(&self) -> &'static str {
        status_of(self.ok, self.fail)
    }
    pub fn actor_label(&self) -> String {
        self.last_actor.clone().unwrap_or_else(|| "human".into())
    }
    pub fn is_agent(&self) -> bool {
        self.last_actor.as_deref().is_some_and(|a| a.starts_with("agent"))
    }
    /// Share of runs with a KNOWN outcome that succeeded; None when no exit code was ever
    /// captured (old imports) - that's "unknown", not "0% ok".
    pub fn success_rate(&self) -> Option<f64> {
        let known = self.ok + self.fail;
        (known > 0).then(|| ((self.ok as f64) / (known as f64) * 100.0).round() / 100.0)
    }
}

pub fn status_of(ok: u32, fail: u32) -> &'static str {
    match (ok > 0, fail > 0) {
        (true, false) => "ok",
        (false, true) => "fail",
        (true, true) => "mixed",
        _ => "unknown",
    }
}

#[derive(Default)]
struct Interner {
    names: Vec<String>,
    index: HashMap<String, u32>,
}

impl Interner {
    fn get(&self, k: &str) -> Option<u32> {
        self.index.get(k).copied()
    }
    fn intern(&mut self, k: &str, display: &str) -> u32 {
        if let Some(i) = self.index.get(k) {
            return *i;
        }
        let i = self.names.len() as u32;
        self.names.push(display.to_string());
        self.index.insert(k.to_string(), i);
        i
    }
}

#[derive(Default)]
pub struct Store {
    pub entries: Vec<Entry>,
    /// entries.len() * DIM, L2-normalised; zero rows for entries without a vector
    pub vecs: Vec<f32>,
    pub desc_vecs: Vec<f32>,
    pub desc_owner: Vec<u32>,
    pub by_text: HashMap<String, u32>,
    pub by_id: HashMap<i64, u32>,
    cwds: Interner,
    cwd_repo: Vec<u32>,
    repos: Interner,
    sessions: Interner,
    pub execs: Vec<Exec>,
}

/// One bit per character class: a-z, 0-9, and the other printable ASCII folded into the rest.
pub fn char_bit(c: u8) -> u64 {
    let c = c.to_ascii_lowercase();
    let i = match c {
        b'a'..=b'z' => c - b'a',
        b'0'..=b'9' => 26 + c - b'0',
        33..=126 => 36 + (c % 28),
        _ => return 0,
    };
    1u64 << i
}

pub fn char_mask(t: &str) -> u64 {
    t.bytes().fold(0, |m, b| m | char_bit(b))
}

fn is_comment(t: &str) -> bool {
    t.trim_start().starts_with('#')
}

pub fn is_self_ref(text: &str) -> bool {
    // (not describe::parse_cmd: it reads a `cd` target like /c/x/reman as the program name)
    let (prog, _) = describe::parse_cmd(text);
    let l = text.to_lowercase();
    // reman invoked anywhere in a pipeline / chain / loop, not just as the first word
    let invokes = l.contains("reman.exe")
        || l.contains("reman-hook")
        || l.contains(".reman/bin")
        || l.contains(".reman\\bin")
        || l.split(|c: char| c.is_whitespace() || matches!(c, ';' | '|' | '&' | '(' | '`' | '$' | '{')).any(|t| t == "reman");
    invokes
        || ["reman_tui.py", "reman_daemon.py", "reman_mcp.py", "reman_init.py", "reman_hook_claude.py"].iter().any(|s| l.contains(s))
        || (prog.as_deref() == Some("python") && l.contains("reman.py"))
}

/// Source code pasted into a prompt (PSReadLine records every pasted line as a "command"):
/// Python/JS statements, fragments of a multi-line block, bare number lists. Shell lines that
/// merely look similar (`FOO=1 cmd`, `for f in *; do`, `$x = 1`) are not flagged.
pub fn looks_like_code(text: &str) -> bool {
    use std::sync::OnceLock;
    static KW: OnceLock<regex::Regex> = OnceLock::new();
    static ASSIGN: OnceLock<regex::Regex> = OnceLock::new();
    let kw = KW.get_or_init(|| {
        regex::Regex::new(
            r"^(?:async\s+def\b|def\s|class\s|return\b|await\s|elif\b|else\s*:|try\s*:|except\b|finally\s*:|yield\b|raise\b|pass$|break$|continue$|lambda\b|from\s+\S+\s+import\s|import\s+[\w.]+(?:\s+as\s+\w+)?$|print\(|console\.log\(|const\s|let\s|var\s|if\s.*:$|for\s+[\w, ()]+\s+in\s.*:$|while\s.*:$|with\s.*:$|@\w+(?:\(.*\))?$)",
        )
        .unwrap()
    });
    let assign = ASSIGN.get_or_init(|| regex::Regex::new(r#"^[A-Za-z_][\w.\[\]'"]*\s+(?:[+\-*/]?=)\s+\S"#).unwrap());
    let first = text.trim().lines().next().unwrap_or("").trim();
    if first.is_empty() {
        return true;
    }
    let c0 = first.chars().next().unwrap();
    if matches!(c0, '.' | ')' | ']' | '}' | ',') && !first.starts_with("./") && !first.starts_with(".\\") && !first.starts_with("..") {
        return true;
    }
    if kw.is_match(first) || assign.is_match(first) {
        return true;
    }
    if first.chars().all(|c| c.is_ascii_digit() || c.is_whitespace() || matches!(c, '.' | ',' | '-')) {
        return true;
    }
    // an unfinished block line: `ports:`, `foo(`, `[`
    first.ends_with([':', '(', ',', '[']) && !(first.len() == 2 && first.as_bytes()[0].is_ascii_alphabetic())
}

impl Store {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn alive_count(&self) -> usize {
        self.entries.iter().filter(|e| e.alive).count()
    }

    pub fn load(db: &Connection) -> Result<Self> {
        let mut s = Store::default();
        let mut vec_by_id: HashMap<i64, Vec<u8>> = HashMap::new();
        {
            let mut st = db.prepare("SELECT command_id, vec FROM command_vec")?;
            let mut rows = st.query([])?;
            while let Some(r) = rows.next()? {
                vec_by_id.insert(r.get(0)?, r.get(1)?);
            }
        }
        let mut st = db.prepare(
            "SELECT id, cmd_text, cwd, run_count, success_count, fail_count, human_runs, agent_runs,
                    last_used, last_exit, last_actor, description, pinned
             FROM commands ORDER BY id",
        )?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let id: i64 = r.get(0)?;
            let text: String = r.get::<_, Option<String>>(1)?.unwrap_or_default();
            if text.trim().is_empty() {
                continue;
            }
            let cwd: Option<String> = r.get(2)?;
            let cwd_i = cwd.as_deref().map(|c| s.intern_cwd(c));
            let row = Row {
                id,
                cwd: cwd_i,
                runs: r.get::<_, Option<i64>>(3)?.unwrap_or(1).max(0) as u32,
                ok: r.get::<_, Option<i64>>(4)?.unwrap_or(0).max(0) as u32,
                fail: r.get::<_, Option<i64>>(5)?.unwrap_or(0).max(0) as u32,
                human: r.get::<_, Option<i64>>(6)?.unwrap_or(0).max(0) as u32,
                agent: r.get::<_, Option<i64>>(7)?.unwrap_or(0).max(0) as u32,
                last_used: r.get::<_, Option<i64>>(8)?.unwrap_or(0),
                last_exit: r.get(9)?,
                last_actor: r.get(10)?,
            };
            let desc: Option<String> = r.get::<_, Option<String>>(11)?.filter(|d| !d.is_empty());
            let pinned = r.get::<_, Option<i64>>(12)?.unwrap_or(0) != 0;
            let vec = vec_by_id.get(&id).and_then(|b| db::unpack(b));
            let ei = s.upsert_entry(&text, vec.as_deref(), desc);
            let e = &mut s.entries[ei as usize];
            e.pinned |= pinned;
            e.rows.push(row);
            s.by_id.insert(id, ei);
        }
        drop(rows);
        drop(st);
        // description vectors: one set per entry (every row of a text shares the same descriptions)
        let mut seen: HashMap<(u32, String), ()> = HashMap::new();
        let mut st = db.prepare("SELECT command_id, kind, vec FROM command_desc_vec")?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let cid: i64 = r.get(0)?;
            let kind: String = r.get::<_, Option<String>>(1)?.unwrap_or_default();
            let Some(&ei) = s.by_id.get(&cid) else { continue };
            if seen.insert((ei, kind), ()).is_some() {
                continue;
            }
            if let Some(mut v) = db::unpack(&r.get::<_, Vec<u8>>(2)?) {
                crate::embed::normalize(&mut v);
                s.desc_vecs.extend_from_slice(&v);
                s.desc_owner.push(ei);
            }
        }
        drop(rows);
        drop(st);
        let mut st = db.prepare("SELECT command_id, ts, cwd, session, exit FROM executions WHERE ts IS NOT NULL ORDER BY ts, id")?;
        let mut rows = st.query([])?;
        while let Some(r) = rows.next()? {
            let cid: i64 = r.get(0)?;
            let Some(&ei) = s.by_id.get(&cid) else { continue };
            let cwd: Option<String> = r.get(2)?;
            let sess: Option<String> = r.get(3)?;
            let ex = Exec {
                ts: r.get(1)?,
                entry: ei,
                row_id: cid,
                cwd: cwd.as_deref().map(|c| s.intern_cwd(c)),
                session: s.sessions.intern(sess.as_deref().unwrap_or(""), sess.as_deref().unwrap_or("")),
                exit: r.get(4)?,
            };
            s.execs.push(ex);
        }
        for i in 0..s.entries.len() {
            let e = &s.entries[i];
            let n = looks_like_code(&e.text) && e.rows.iter().all(|r| r.ok + r.fail == 0 && s.row_unscoped(r));
            s.entries[i].noise = n;
        }
        Ok(s)
    }

    fn intern_cwd(&mut self, c: &str) -> u32 {
        let n = config::norm_path(c);
        if let Some(i) = self.cwds.get(&n) {
            return i;
        }
        let i = self.cwds.intern(&n, c);
        let repo = describe::repo_identity(c);
        let ri = self.repos.intern(&repo, &repo);
        self.cwd_repo.push(ri);
        debug_assert_eq!(self.cwd_repo.len(), i as usize + 1);
        i
    }

    /// A row with no recorded folder (history imported from before reman captured folders).
    pub fn row_unscoped(&self, r: &Row) -> bool {
        r.cwd.is_none_or(|c| {
            let n = self.cwd_name(c);
            n.is_empty() || n.eq_ignore_ascii_case("unknown")
        })
    }

    pub fn cwd_name(&self, i: u32) -> &str {
        &self.cwds.names[i as usize]
    }

    pub fn session_name(&self, i: u32) -> &str {
        &self.sessions.names[i as usize]
    }

    pub fn session_index(&self, name: &str) -> Option<u32> {
        self.sessions.get(name)
    }

    pub fn cwd_index(&self, path: &str) -> Option<u32> {
        self.cwds.get(&config::norm_path(path))
    }

    pub fn scope_folder(&self, path: &str) -> Scope {
        self.cwd_index(path).map(Scope::Folder).unwrap_or(Scope::Nothing)
    }

    pub fn scope_repo(&self, path: &str) -> Scope {
        let ident = describe::repo_identity(path);
        self.repos.get(&ident).map(Scope::Repo).unwrap_or(Scope::Nothing)
    }

    pub fn row_in_scope(&self, r: &Row, scope: Scope) -> bool {
        match scope {
            Scope::All => true,
            Scope::Folder(f) => r.cwd == Some(f),
            Scope::Repo(rp) => r.cwd.is_some_and(|c| self.cwd_repo[c as usize] == rp),
            Scope::Nothing => false,
        }
    }

    pub fn in_scope(&self, e: &Entry, scope: Scope) -> bool {
        scope == Scope::All || e.rows.iter().any(|r| self.row_in_scope(r, scope))
    }

    /// Aggregate provenance over the rows that fall inside `scope`.
    pub fn agg(&self, e: &Entry, scope: Scope) -> Agg {
        let mut a = Agg::default();
        let mut newest = i64::MIN;
        for r in e.rows.iter().filter(|r| self.row_in_scope(r, scope)) {
            a.runs += r.runs;
            a.ok += r.ok;
            a.fail += r.fail;
            a.human += r.human;
            a.agent += r.agent;
            a.folders += r.cwd.is_some() as u32;
            if r.last_used > newest {
                newest = r.last_used;
                a.last_used = r.last_used;
                a.last_exit = r.last_exit;
                a.last_actor = r.last_actor.clone();
                a.cwd = r.cwd;
            }
        }
        a
    }

    /// Filter helper shared by search/browse: actor = human|agent, status = ok|fail.
    pub fn passes(&self, a: &Agg, actor: Option<&str>, status: Option<&str>) -> bool {
        let actor_ok = match actor {
            Some("agent") => a.agent > 0 || a.is_agent(),
            Some("human") => a.human > 0 || (a.agent == 0 && !a.is_agent()),
            _ => true,
        };
        let status_ok = match status {
            Some("ok") => a.ok > 0,
            Some("fail") => a.fail > 0 && a.ok == 0,
            _ => true,
        };
        actor_ok && status_ok && a.runs > 0
    }

    pub fn vec(&self, i: u32) -> &[f32] {
        &self.vecs[i as usize * DIM..(i as usize + 1) * DIM]
    }

    fn upsert_entry(&mut self, text: &str, vec: Option<&[f32]>, desc: Option<String>) -> u32 {
        if let Some(&i) = self.by_text.get(text) {
            let e = &mut self.entries[i as usize];
            if !e.has_vec {
                if let Some(v) = vec {
                    let mut v = v.to_vec();
                    crate::embed::normalize(&mut v);
                    self.vecs[i as usize * DIM..(i as usize + 1) * DIM].copy_from_slice(&v);
                    e.has_vec = true;
                }
            }
            if e.desc.is_none() {
                e.desc = desc;
            }
            return i;
        }
        let i = self.entries.len() as u32;
        match vec {
            Some(v) => {
                let mut v = v.to_vec();
                crate::embed::normalize(&mut v);
                self.vecs.extend_from_slice(&v);
            }
            None => self.vecs.extend(std::iter::repeat_n(0.0, DIM)),
        }
        self.entries.push(Entry {
            text: text.to_string(),
            lower: text.to_lowercase(),
            mask: char_mask(text),
            gkey: describe::group_key(text),
            shape: describe::shape_key(text),
            desc,
            rows: Vec::new(),
            pinned: false,
            comment: is_comment(text),
            self_ref: is_self_ref(text),
            alive: true,
            has_vec: vec.is_some(),
            noise: false,
        });
        self.by_text.insert(text.to_string(), i);
        i
    }

    pub fn knows(&self, text: &str) -> bool {
        self.by_text.contains_key(text)
    }

    /// Mirror a db::record_run into memory. `vecs` = (raw, desc display, desc part vectors) for a
    /// brand-new text. Returns the entry index.
    pub fn apply_run(&mut self, command_id: i64, new_row: bool, run: &Run, vecs: Option<(&[f32], Option<String>, &[Vec<f32>])>) -> u32 {
        let ei = match self.by_text.get(&run.cmd) {
            Some(&i) => i,
            None => {
                let (raw, desc, parts) = match vecs {
                    Some((raw, d, p)) => (Some(raw), d, p),
                    None => (None, None, &[][..]),
                };
                let i = self.upsert_entry(&run.cmd, raw, desc);
                for v in parts {
                    let mut v = v.clone();
                    crate::embed::normalize(&mut v);
                    self.desc_vecs.extend_from_slice(&v);
                    self.desc_owner.push(i);
                }
                i
            }
        };
        let cwd = run.cwd.as_deref().map(|c| self.intern_cwd(c));
        let session = self.sessions.intern(&run.session, &run.session);
        let (ok, bad) = (run.ok() as u32, run.failed() as u32);
        let (h, a) = if run.is_agent() { (0, 1) } else { (1, 0) };
        let e = &mut self.entries[ei as usize];
        e.alive = true;
        e.noise = false; // it just ran for real
        if new_row {
            e.rows.push(Row {
                id: command_id,
                cwd,
                runs: 1,
                ok,
                fail: bad,
                human: h,
                agent: a,
                last_used: run.ts,
                last_exit: run.exit,
                last_actor: Some(run.actor.clone()),
            });
        } else if let Some(r) = e.rows.iter_mut().find(|r| r.id == command_id) {
            r.runs += 1;
            r.ok += ok;
            r.fail += bad;
            r.human += h;
            r.agent += a;
            r.last_used = r.last_used.max(run.ts);
            r.last_exit = run.exit;
            r.last_actor = Some(run.actor.clone());
        }
        self.by_id.insert(command_id, ei);
        let ex = Exec { ts: run.ts, entry: ei, row_id: command_id, cwd, session, exit: run.exit };
        // imports can arrive out of order; keep the log sorted by ts
        if self.execs.last().is_none_or(|l| l.ts <= ex.ts) {
            self.execs.push(ex);
        } else {
            let pos = self.execs.partition_point(|x| x.ts <= ex.ts);
            self.execs.insert(pos, ex);
        }
        ei
    }

    /// Drop specific (command, cwd) rows (retention purge). Entry dies when its last row goes.
    pub fn remove_rows(&mut self, ids: &[i64]) {
        let set: std::collections::HashSet<i64> = ids.iter().copied().collect();
        for id in ids {
            if let Some(ei) = self.by_id.remove(id) {
                let e = &mut self.entries[ei as usize];
                e.rows.retain(|r| r.id != *id);
                if e.rows.is_empty() && e.alive {
                    e.alive = false;
                    let t = e.text.clone();
                    self.by_text.remove(&t);
                }
            }
        }
        self.execs.retain(|x| !set.contains(&x.row_id));
    }

    /// Row ids of a text (for `forget`).
    pub fn row_ids(&self, text: &str) -> Vec<i64> {
        self.by_text.get(text).map(|&i| self.entries[i as usize].rows.iter().map(|r| r.id).collect()).unwrap_or_default()
    }

    pub fn set_pinned(&mut self, text: &str, on: bool) -> bool {
        match self.by_text.get(text) {
            Some(&i) => {
                self.entries[i as usize].pinned = on;
                true
            }
            None => false,
        }
    }

    pub fn entry(&self, text: &str) -> Option<(u32, &Entry)> {
        self.by_text.get(text).map(|&i| (i, &self.entries[i as usize]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(cmd: &str, exit: i64, cwd: &str, actor: &str, ts: i64) -> Run {
        Run { cmd: cmd.into(), exit: Some(exit), cwd: Some(cwd.into()), session: "s".into(), actor: actor.into(), ts, duration_ms: None }
    }

    #[test]
    fn mirrors_db_and_scopes() {
        let db = Connection::open_in_memory().unwrap();
        db::migrate(&db).unwrap();
        let mut s = Store::default();
        for r in [run("git status", 0, "C:/a", "human", 1), run("git status", 1, "C:/b", "agent:x", 2), run("ls", 0, "C:/a", "human", 3)] {
            let rec = db::record_run(&db, &r).unwrap();
            s.apply_run(rec.command_id, rec.new_row, &r, Some((&vec![1.0; DIM], None, &[])));
        }
        let loaded = Store::load(&db).unwrap();
        for st in [&s, &loaded] {
            let (_, e) = st.entry("git status").unwrap();
            let all = st.agg(e, Scope::All);
            assert_eq!((all.runs, all.ok, all.fail, all.status()), (2, 1, 1, "mixed"));
            // spelled as recorded: folder case only folds on Windows
            let fa = st.scope_folder("C:/a");
            assert_eq!(st.agg(e, fa).status(), "ok");
            assert_eq!(st.execs.len(), 3);
        }
        s.remove_rows(&s.row_ids("ls"));
        assert!(!s.knows("ls"));
        assert_eq!(s.execs.len(), 2);
    }
}

#[cfg(test)]
mod noise_tests {
    use super::looks_like_code;

    #[test]
    fn pasted_code_vs_shell() {
        for c in ["return db_obj", "async def update(", "for i in range(1, n + 1):", ".offset(skip)", "obj_in_data = obj_in",
                  "from sqlalchemy import or_", "6 4 2 1 2 3", "ports:", "result = await db.execute(query)", "};", "print(*arr1)"] {
            assert!(looks_like_code(c), "{c}");
        }
        for c in ["docker-compose up -d", "FOO=1 npm test", "for f in *.py; do echo $f; done", "$x = 1", "d:", "./run.sh",
                  "../x/build.ps1", "python -c \"import x\"", "git commit -m \"fix: y\"", "if (Test-Path x) { rm x }", "import-module posh-git"] {
            assert!(!looks_like_code(c), "{c}");
        }
    }
}

#[cfg(test)]
mod self_ref_tests {
    use super::is_self_ref;

    #[test]
    fn flags_reman_invocations_only() {
        assert!(is_self_ref(r#"reman search "tear down containers""#));
        assert!(is_self_ref(r#"& "C:\py\python.exe" "C:\x\reman_tui.py" --query q"#));
        assert!(is_self_ref(r#"for q in "a" "b"; do ~/.reman/bin/reman.exe search "$q"; done"#));
        assert!(is_self_ref("cd /c/x && reman stats"));
        assert!(!is_self_ref("cd /c/Users/rdars/reman && cargo build --release"));
        assert!(!is_self_ref("git clone https://github.com/x/remanufacture"));
        assert!(!is_self_ref("docker-compose down"));
    }
}
