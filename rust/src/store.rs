//! In-memory model of the whole history. Everything the hot path needs (vectors, provenance,
//! per-folder stats, the execution log) lives here, so search / browse / did-you-mean never touch
//! SQLite. SQLite is the durable copy; the daemon mirrors every write into this struct.
use crate::config::{self, DIM};
use crate::db::{self, Run};
use crate::describe;
use anyhow::Result;
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

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
    pub last_actor: Option<Arc<str>>,
    /// how long its recent successful and failed runs took, newest last (Row::KEEP_MS each)
    pub ok_ms: Vec<u32>,
    pub fail_ms: Vec<u32>,
}

impl Row {
    const KEEP_MS: usize = 20;

    fn note_ms(&mut self, exit: Option<i64>, ms: Option<i64>) {
        let (Some(x), Some(ms)) = (exit, ms.filter(|m| *m >= 0)) else { return };
        let v = match x {
            0 => &mut self.ok_ms,
            x if x > 0 => &mut self.fail_ms,
            _ => return,
        };
        v.reserve_exact(1);
        v.push(ms.min(u32::MAX as i64) as u32);
        if v.len() > Self::KEEP_MS {
            v.remove(0);
        }
    }
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
    /// every description sentence (the tool's and the subcommand's), lowercased once: the
    /// words search matches a query against, on every keystroke
    pub desc_words: Option<Arc<str>>,
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
    /// the programs it runs (describe::programs): `npx astro dev` -> npx, astro
    pub progs: Box<[Arc<str>]>,
    /// what its description says it does to its object (describe::Action)
    pub action: Option<describe::Action>,
    /// the last part of each path in it, lowercased (`tar -czf b.tgz data/`: "b.tgz data"):
    /// words search matches a query against (search::says_word)
    pub path_ends: Box<str>,
}

/// The last part of each path in a command, lowercased, space-separated.
fn path_ends(lower: &str) -> Box<str> {
    lower
        .split_whitespace()
        .filter(|t| t.contains(['/', '\\']))
        .map(|t| t.trim_matches(|c: char| matches!(c, '"' | '\'' | '/' | '\\')).rsplit(['/', '\\']).next().unwrap_or(""))
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .into()
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
    /// the git branch and commit checked out when it ran: Store::git_name; 0 = not known
    pub branch: u32,
    pub head: u32,
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
    pub last_actor: Option<Arc<str>>,
    pub cwd: Option<u32>,
    pub folders: u32,
}

impl Agg {
    pub fn status(&self) -> &'static str {
        status_of(self.ok, self.fail)
    }
    pub fn actor_label(&self) -> String {
        self.last_actor.as_deref().unwrap_or("human").to_string()
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
    names: Vec<Arc<str>>,
    /// (the same string as the name, when the key is the name itself)
    index: HashMap<Arc<str>, u32>,
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
        let name: Arc<str> = display.into();
        self.index.insert(if k == display { name.clone() } else { k.into() }, i);
        self.names.push(name);
        i
    }
}

#[derive(Default)]
pub struct Store {
    pub entries: Vec<Entry>,
    /// entries.len() * DIM, L2-normalised, as half floats (f16.rs); zero rows for entries without
    /// a vector
    pub vecs: Vec<u16>,
    pub desc: DescVecs,
    by_text: TextIndex,
    pub by_id: HashMap<i64, u32>,
    cwds: Interner,
    cwd_repo: Vec<u32>,
    repos: Interner,
    sessions: Interner,
    /// branch names and commits (Exec::branch, Exec::head)
    git: Interner,
    pub execs: Vec<Exec>,
    /// entry -> what it printed the last time it failed (redacted, short; see errors.rs)
    pub last_err: HashMap<u32, String>,
    /// error signature -> the entries that failed that way (errors::signature)
    pub by_sig: HashMap<String, Vec<u32>>,
    /// every program some command in the history runs: a query naming one names a tool you use
    pub programs: HashSet<Arc<str>>,
    /// one copy of each string many commands hold (who ran it last, what a description says)
    strs: HashSet<Arc<str>>,
    /// (entry, folder, command of the line) -> [runs, ok, fail] to add to what the line's exit
    /// status recorded, from what its output said (verdict.rs)
    pub seen: HashMap<(u32, Option<u32>, String), [i32; 3]>,
}

/// `s` as the one copy `pool` holds.
fn shared(pool: &mut HashSet<Arc<str>>, s: &str) -> Arc<str> {
    if let Some(a) = pool.get(s) {
        return a.clone();
    }
    let a: Arc<str> = s.into();
    pool.insert(a.clone());
    a
}

/// Every sentence of a description, lowercased: Entry::desc_words.
fn described_words(d: &describe::Description) -> Option<String> {
    Some(d.parts.iter().map(|p| p.1.to_lowercase()).collect::<Vec<_>>().join(" ; ")).filter(|w| !w.is_empty())
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

/// text -> entry, without a second copy of every text: keyed by the text's hash and checked
/// against the entry's own text. A different text with the same hash (one in billions) goes in
/// `clash`, keyed by itself.
#[derive(Default)]
struct TextIndex {
    hasher: std::hash::RandomState,
    by_hash: HashMap<u64, u32>,
    clash: HashMap<String, u32>,
}

impl TextIndex {
    fn get(&self, entries: &[Entry], text: &str) -> Option<u32> {
        use std::hash::BuildHasher;
        match self.by_hash.get(&self.hasher.hash_one(text)) {
            Some(&i) if entries[i as usize].text == text => Some(i),
            _ => self.clash.get(text).copied(),
        }
    }

    fn insert(&mut self, entries: &[Entry], text: &str, i: u32) {
        use std::hash::BuildHasher;
        let h = self.hasher.hash_one(text);
        match self.by_hash.get(&h) {
            Some(&j) if entries[j as usize].text != text => {
                self.clash.insert(text.to_string(), i);
            }
            _ => {
                self.by_hash.insert(h, i);
            }
        }
    }

    fn remove(&mut self, entries: &[Entry], text: &str) {
        use std::hash::BuildHasher;
        let h = self.hasher.hash_one(text);
        if self.by_hash.get(&h).is_some_and(|&i| entries[i as usize].text == text) {
            self.by_hash.remove(&h);
        }
        self.clash.remove(text);
    }
}

/// The description vectors, each distinct one held once: commands that run the same tool share
/// its sentences (one history: 11,459 (command, sentence) pairs, 575 distinct vectors).
#[derive(Default)]
pub struct DescVecs {
    /// the distinct vectors, DIM half floats each, L2-normalised
    vecs: Vec<u16>,
    /// (entry, vector) pairs: entry owner[k] is described by vector of[k]
    owner: Vec<u32>,
    of: Vec<u32>,
    /// a vector's hash -> the vector. A different vector with the same hash is just held again.
    hasher: std::hash::RandomState,
    index: HashMap<u64, u32>,
}

impl DescVecs {
    /// How many distinct vectors.
    pub fn len(&self) -> usize {
        self.vecs.len() / DIM
    }

    pub fn get(&self, j: u32) -> &[u16] {
        &self.vecs[j as usize * DIM..(j as usize + 1) * DIM]
    }

    /// (entry, vector index) for every description vector of every entry.
    pub fn pairs(&self) -> impl Iterator<Item = (u32, u32)> + '_ {
        self.owner.iter().copied().zip(self.of.iter().copied())
    }

    /// One of `entry`'s description vectors, as embedded.
    fn add(&mut self, entry: u32, v: &[f32]) {
        use std::hash::BuildHasher;
        let mut v = v.to_vec();
        crate::embed::normalize(&mut v);
        let at = self.vecs.len();
        crate::f16::encode_into(&mut self.vecs, &v);
        let h = self.hasher.hash_one(&self.vecs[at..]);
        let j = match self.index.get(&h) {
            Some(&j) if self.get(j) == &self.vecs[at..] => {
                self.vecs.truncate(at);
                j
            }
            Some(_) => (at / DIM) as u32,
            None => {
                self.index.insert(h, (at / DIM) as u32);
                (at / DIM) as u32
            }
        };
        self.owner.push(entry);
        self.of.push(j);
    }

    /// Every vector of these entries out; one no entry has any more goes too.
    fn remove(&mut self, entries: &HashSet<u32>) {
        use std::hash::BuildHasher;
        let mut keep = 0;
        for k in 0..self.owner.len() {
            if !entries.contains(&self.owner[k]) {
                self.owner[keep] = self.owner[k];
                self.of[keep] = self.of[k];
                keep += 1;
            }
        }
        self.owner.truncate(keep);
        self.of.truncate(keep);
        let mut used = vec![false; self.len()];
        for &j in &self.of {
            used[j as usize] = true;
        }
        let mut moved_to = vec![0u32; used.len()];
        let mut next = 0;
        for (j, _) in used.iter().enumerate().filter(|u| *u.1) {
            if next != j {
                self.vecs.copy_within(j * DIM..(j + 1) * DIM, next * DIM);
            }
            moved_to[j] = next as u32;
            next += 1;
        }
        self.vecs.truncate(next * DIM);
        for j in &mut self.of {
            *j = moved_to[*j as usize];
        }
        self.index.clear();
        for j in 0..next as u32 {
            self.index.entry(self.hasher.hash_one(self.get(j))).or_insert(j);
        }
    }

    fn shrink_to_fit(&mut self) {
        self.vecs.shrink_to_fit();
        self.owner.shrink_to_fit();
        self.of.shrink_to_fit();
        self.index.shrink_to_fit();
    }
}

/// Room for one more vector: an eighth more at a time, not double (these are the big arrays, and
/// a new command arrives one at a time)
fn room(v: &mut Vec<u16>) {
    if v.capacity() - v.len() < DIM {
        v.reserve_exact((v.len() / 8).max(DIM * 64));
    }
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
    // a YAML list item (`- "8000:8000"`) or `key: value` line (`restart: unless-stopped`) from
    // a pasted docker-compose or config file; a drive (`d: `) is no key
    if first.starts_with("- ") || first.split_once(": ").is_some_and(|(k, _)| k.len() > 1 && k.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))) {
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
        // every command is described below: with what this machine says of tldr's unknowns
        if let Ok((said, _)) = db::program_help(db) {
            describe::set_local(said);
        }
        // sized once: grown by doubling, the vectors would hold up to twice their size
        let n: i64 = db.query_row("SELECT count(*) FROM commands", [], |r| r.get(0))?;
        s.vecs.reserve_exact(n.max(0) as usize * DIM);
        // each vector read with its row (not all of them gathered first: ~1.5 KB each)
        let mut st = db.prepare(
            "SELECT c.id, c.cmd_text, c.cwd, c.run_count, c.success_count, c.fail_count, c.human_runs, c.agent_runs,
                    c.last_used, c.last_exit, c.last_actor, c.description, c.pinned, v.vec
             FROM commands c LEFT JOIN command_vec v ON v.command_id = c.id ORDER BY c.id",
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
                last_actor: r.get::<_, Option<String>>(10)?.map(|a| shared(&mut s.strs, &a)),
                ok_ms: Vec::new(),
                fail_ms: Vec::new(),
            };
            let desc: Option<String> = r.get::<_, Option<String>>(11)?.filter(|d| !d.is_empty());
            let pinned = r.get::<_, Option<i64>>(12)?.unwrap_or(0) != 0;
            let vec = r.get_ref(13)?.as_blob_or_null()?.and_then(db::unpack);
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
            if let Some(v) = db::unpack(&r.get::<_, Vec<u8>>(2)?) {
                s.desc.add(ei, &v);
            }
        }
        drop(rows);
        drop(st);
        let mut st = db.prepare("SELECT command_id, ts, cwd, session, exit, err, seen, branch, head, duration_ms FROM executions WHERE ts IS NOT NULL ORDER BY ts, id")?;
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
                branch: s.git_ref(r.get::<_, Option<String>>(7)?.as_deref()),
                head: s.git_ref(r.get::<_, Option<String>>(8)?.as_deref()),
            };
            let err: Option<String> = r.get(5)?;
            if let Some(e) = err.filter(|_| ex.exit.is_some_and(|x| x != 0)) {
                s.note_error(ei, e);
            }
            if let Some(j) = r.get::<_, Option<String>>(6)? {
                s.note_seen(ei, ex.cwd, &j);
            }
            if let Some(row) = s.entries[ei as usize].rows.iter_mut().find(|w| w.id == cid) {
                row.note_ms(ex.exit, r.get(9)?);
            }
            s.execs.push(ex);
        }
        s.vecs.shrink_to_fit();
        s.desc.shrink_to_fit();
        s.execs.shrink_to_fit();
        s.entries.shrink_to_fit();
        // most commands have one row and a few runs: a Vec grown one push at a time holds 4+
        for e in &mut s.entries {
            e.rows.shrink_to_fit();
            for r in &mut e.rows {
                r.ok_ms.shrink_to_fit();
                r.fail_ms.shrink_to_fit();
            }
        }
        for i in 0..s.entries.len() {
            let e = &s.entries[i];
            let n = looks_like_code(&e.text) && e.rows.iter().all(|r| r.ok + r.fail == 0 && s.row_unscoped(r));
            s.entries[i].noise = n;
        }
        Ok(s)
    }

    fn git_ref(&mut self, v: Option<&str>) -> u32 {
        v.map_or(0, |x| self.git.intern(x, x) + 1)
    }

    /// A branch name or commit as Exec::branch / Exec::head hold it.
    pub fn git_name(&self, i: u32) -> Option<&str> {
        (i > 0).then(|| &*self.git.names[i as usize - 1])
    }

    fn intern_cwd(&mut self, c: &str) -> u32 {
        let n = config::norm_path(c);
        if let Some(i) = self.cwds.get(&n) {
            return i;
        }
        let i = self.cwds.intern(&n, c);
        let repo = crate::git::repo_identity(c);
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

    /// How many folders the history knows (cwd indices are 0..this).
    pub fn cwd_count(&self) -> u32 {
        self.cwds.names.len() as u32
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
        let ident = crate::git::repo_identity(path);
        self.repos.get(&ident).map(Scope::Repo).unwrap_or(Scope::Nothing)
    }

    pub fn row_in_scope(&self, r: &Row, scope: Scope) -> bool {
        self.cwd_in_scope(r.cwd, scope)
    }

    pub fn cwd_in_scope(&self, cwd: Option<u32>, scope: Scope) -> bool {
        match scope {
            Scope::All => true,
            Scope::Folder(f) => cwd == Some(f),
            Scope::Repo(rp) => cwd.is_some_and(|c| self.cwd_repo[c as usize] == rp),
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

    /// How long it usually takes in `scope`: the median of its recent successful (`ok`) or failed
    /// runs there. None when no run there was timed.
    pub fn typical(&self, e: &Entry, scope: Scope, ok: bool) -> Option<u32> {
        let mut v: Vec<u32> = e.rows.iter().filter(|r| self.row_in_scope(r, scope)).flat_map(|r| if ok { &r.ok_ms } else { &r.fail_ms }).copied().collect();
        if v.is_empty() {
            return None;
        }
        v.sort_unstable();
        Some(v[v.len() / 2])
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

    pub fn vec(&self, i: u32) -> &[u16] {
        &self.vecs[i as usize * DIM..(i as usize + 1) * DIM]
    }

    fn upsert_entry(&mut self, text: &str, vec: Option<&[f32]>, desc: Option<String>) -> u32 {
        if let Some(i) = self.by_text.get(&self.entries, text) {
            let e = &mut self.entries[i as usize];
            if !e.has_vec {
                if let Some(v) = vec {
                    let mut v = v.to_vec();
                    crate::embed::normalize(&mut v);
                    for (d, x) in self.vecs[i as usize * DIM..(i as usize + 1) * DIM].iter_mut().zip(&v) {
                        *d = crate::f16::encode(*x);
                    }
                    e.has_vec = true;
                }
            }
            if e.desc.is_none() {
                e.desc = desc;
            }
            return i;
        }
        let i = self.entries.len() as u32;
        room(&mut self.vecs);
        match vec {
            Some(v) => {
                let mut v = v.to_vec();
                crate::embed::normalize(&mut v);
                crate::f16::encode_into(&mut self.vecs, &v);
            }
            None => self.vecs.extend(std::iter::repeat_n(0, DIM)),
        }
        let progs = describe::programs(text).iter().map(|p| shared(&mut self.programs, p)).collect();
        let described = describe::describe(text);
        let desc_words = described_words(&described).map(|w| shared(&mut self.strs, &w));
        let lower = text.to_lowercase();
        self.entries.push(Entry {
            text: text.to_string(),
            path_ends: path_ends(&lower),
            lower,
            mask: char_mask(text),
            gkey: describe::group_key(text),
            shape: describe::shape_key(text),
            desc_words,
            action: describe::command_action(text, &described),
            desc,
            rows: Vec::new(),
            pinned: false,
            comment: is_comment(text),
            self_ref: is_self_ref(text),
            alive: true,
            has_vec: vec.is_some(),
            noise: false,
            progs,
        });
        self.by_text.insert(&self.entries, text, i);
        i
    }

    pub fn knows(&self, text: &str) -> bool {
        self.by_text.get(&self.entries, text).is_some()
    }

    /// Mirror db::write_descriptions into memory: these texts' descriptions replaced by new ones
    /// (display, and one vector per sentence). What a re-description changes, without reloading
    /// the whole history (a reload holds the database for a second or more).
    pub fn set_descriptions(&mut self, updates: &[(&str, Option<String>, Vec<Vec<f32>>)]) {
        let mut replaced: std::collections::HashSet<u32> = std::collections::HashSet::new();
        let found: Vec<(u32, &Option<String>, &Vec<Vec<f32>>)> =
            updates.iter().filter_map(|(t, d, v)| self.by_text.get(&self.entries, t).map(|i| (i, d, v))).collect();
        replaced.extend(found.iter().map(|f| f.0));
        // the old vectors out, in one pass
        self.desc.remove(&replaced);
        for (i, display, vecs) in found {
            for v in vecs {
                self.desc.add(i, v);
            }
            let described = describe::describe(&self.entries[i as usize].text);
            let words = described_words(&described).map(|w| shared(&mut self.strs, &w));
            let e = &mut self.entries[i as usize];
            e.desc_words = words;
            e.action = describe::command_action(&e.text, &described);
            e.desc = display.clone();
        }
        // what no description says any more
        self.strs.retain(|a| Arc::strong_count(a) > 1);
    }

    /// Mirror a db::record_run into memory. `vecs` = (raw, desc display, desc part vectors) for a
    /// brand-new text. Returns the entry index.
    pub fn apply_run(&mut self, command_id: i64, new_row: bool, run: &Run, vecs: Option<(&[f32], Option<String>, &[Vec<f32>])>) -> u32 {
        let ei = match self.by_text.get(&self.entries, &run.cmd) {
            Some(i) => i,
            None => {
                let (raw, desc, parts) = match vecs {
                    Some((raw, d, p)) => (Some(raw), d, p),
                    None => (None, None, &[][..]),
                };
                let i = self.upsert_entry(&run.cmd, raw, desc);
                for v in parts {
                    self.desc.add(i, v);
                }
                i
            }
        };
        let cwd = run.cwd.as_deref().map(|c| self.intern_cwd(c));
        let session = self.sessions.intern(&run.session, &run.session);
        let (ok, bad) = (run.ok() as u32, run.failed() as u32);
        let (h, a) = if run.is_agent() { (0, 1) } else { (1, 0) };
        let actor = shared(&mut self.strs, &run.actor);
        let e = &mut self.entries[ei as usize];
        e.alive = true;
        e.noise = false; // it just ran for real
        if new_row {
            e.rows.reserve_exact(1);
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
                last_actor: Some(actor),
                ok_ms: Vec::new(),
                fail_ms: Vec::new(),
            });
            if let Some(r) = e.rows.last_mut() {
                r.note_ms(run.exit, run.duration_ms);
            }
        } else if let Some(r) = e.rows.iter_mut().find(|r| r.id == command_id) {
            r.note_ms(run.exit, run.duration_ms);
            r.runs += 1;
            r.ok += ok;
            r.fail += bad;
            r.human += h;
            r.agent += a;
            r.last_used = r.last_used.max(run.ts);
            r.last_exit = run.exit;
            r.last_actor = Some(actor);
        }
        self.by_id.insert(command_id, ei);
        if let Some(e) = run.err.clone().filter(|_| run.failed()) {
            self.note_error(ei, e);
        }
        if let Some(j) = &run.seen {
            self.note_seen(ei, cwd, j);
        }
        let (branch, head) = (self.git_ref(run.branch.as_deref()), self.git_ref(run.head.as_deref()));
        let ex = Exec { ts: run.ts, entry: ei, row_id: command_id, cwd, session, exit: run.exit, branch, head };
        // imports can arrive out of order; keep the log sorted by ts
        if self.execs.last().is_none_or(|l| l.ts <= ex.ts) {
            self.execs.push(ex);
        } else {
            let pos = self.execs.partition_point(|x| x.ts <= ex.ts);
            self.execs.insert(pos, ex);
        }
        ei
    }

    /// A run's corrections from what its output said (`[[command, runs, ok, fail], ...]`).
    fn note_seen(&mut self, ei: u32, cwd: Option<u32>, json: &str) {
        self.tally_seen(ei, cwd, json, 1);
    }

    fn tally_seen(&mut self, ei: u32, cwd: Option<u32>, json: &str, sign: i32) {
        let Ok(serde_json::Value::Array(adj)) = serde_json::from_str(json) else { return };
        for a in adj {
            let n = |i: usize| sign * a.get(i).and_then(serde_json::Value::as_i64).unwrap_or(0) as i32;
            if let Some(cmd) = a.get(0).and_then(serde_json::Value::as_str) {
                let t = self.seen.entry((ei, cwd, cmd.to_string())).or_default();
                (t[0], t[1], t[2]) = (t[0] + n(1), t[1] + n(2), t[2] + n(3));
            }
        }
    }

    /// The row (and entry) a command id is, with the row's folder.
    fn row_of(&mut self, command_id: i64) -> Option<(u32, &mut Row)> {
        let ei = *self.by_id.get(&command_id)?;
        let r = self.entries[ei as usize].rows.iter_mut().find(|r| r.id == command_id)?;
        Some((ei, r))
    }

    /// A shell's record of a run became an agent's (db::claim_for_agent), with what its output said.
    pub fn claim_for_agent(&mut self, command_id: i64, actor: &str, seen: Option<&str>) {
        let actor = shared(&mut self.strs, actor);
        let Some((ei, r)) = self.row_of(command_id) else { return };
        r.human = r.human.saturating_sub(1);
        r.agent += 1;
        r.last_actor = Some(actor);
        let cwd = r.cwd;
        if let Some(j) = seen {
            self.note_seen(ei, cwd, j);
        }
    }

    /// An agent's record of a run learned the shell's exit code (db::settle_twin): it counts, and
    /// what the output said is superseded.
    pub fn settle_twin(&mut self, command_id: i64, ts: i64, had: Option<i64>, exit: Option<i64>, seen: Option<&str>) {
        let (Some(x), None) = (exit.filter(|x| *x >= 0), had) else { return };
        let Some((ei, r)) = self.row_of(command_id) else { return };
        r.ok += (x == 0) as u32;
        r.fail += (x > 0) as u32;
        r.last_exit = Some(x);
        let cwd = r.cwd;
        if let Some(j) = seen {
            self.tally_seen(ei, cwd, j, -1);
        }
        if let Some(e) = self.execs.iter_mut().rev().find(|e| e.row_id == command_id && e.ts == ts) {
            e.exit = Some(x);
        }
    }

    /// A failure's output: the entry's last error, and its signature in the index.
    fn note_error(&mut self, ei: u32, err: String) {
        if let Some(sig) = crate::errors::signature(&err) {
            let v = self.by_sig.entry(sig).or_default();
            if !v.contains(&ei) {
                v.push(ei);
            }
        }
        self.last_err.insert(ei, err);
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
                    self.by_text.remove(&self.entries, &self.entries[ei as usize].text);
                }
            }
        }
        self.execs.retain(|x| !set.contains(&x.row_id));
    }

    /// One run forgotten (the inspector's Del): its row's counts follow; `gone` when it was the
    /// row's last run, which takes the row with it.
    pub fn remove_exec(&mut self, row_id: i64, ts: i64, exit: Option<i64>, agent: bool, gone: bool) {
        if gone {
            return self.remove_rows(&[row_id]);
        }
        if let Some(p) = self.execs.iter().rposition(|x| x.row_id == row_id && x.ts == ts) {
            self.execs.remove(p);
        }
        let Some(&ei) = self.by_id.get(&row_id) else { return };
        let latest = self.execs.iter().rev().find(|x| x.row_id == row_id).map(|x| (x.ts, x.exit));
        let Some(r) = self.entries[ei as usize].rows.iter_mut().find(|r| r.id == row_id) else { return };
        let sub = |v: &mut u32, by: bool| *v = v.saturating_sub(by as u32);
        sub(&mut r.runs, true);
        sub(&mut r.ok, exit == Some(0));
        sub(&mut r.fail, exit.is_some_and(|e| e > 0));
        sub(&mut r.human, !agent);
        sub(&mut r.agent, agent);
        if let Some((t, x)) = latest.filter(|_| r.last_used <= ts) {
            r.last_used = t;
            r.last_exit = x;
        }
    }

    /// Row ids of a text (for `forget`).
    pub fn row_ids(&self, text: &str) -> Vec<i64> {
        self.by_text.get(&self.entries, text).map(|i| self.entries[i as usize].rows.iter().map(|r| r.id).collect()).unwrap_or_default()
    }

    pub fn set_pinned(&mut self, text: &str, on: bool) -> bool {
        match self.by_text.get(&self.entries, text) {
            Some(i) => {
                self.entries[i as usize].pinned = on;
                true
            }
            None => false,
        }
    }

    pub fn entry(&self, text: &str) -> Option<(u32, &Entry)> {
        self.by_text.get(&self.entries, text).map(|i| (i, &self.entries[i as usize]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(cmd: &str, exit: i64, cwd: &str, actor: &str, ts: i64) -> Run {
        Run { cmd: cmd.into(), exit: Some(exit), cwd: Some(cwd.into()), session: "s".into(), actor: actor.into(), ts, duration_ms: None, ..Default::default() }
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

    #[test]
    fn descriptions_replaced_in_place() {
        let mut s = Store::default();
        let one = |x: f32| {
            let mut v = vec![0.0; DIM];
            v[0] = x;
            v[1] = 1.0;
            v
        };
        let a = s.apply_run(1, true, &run("eas build", 0, "C:/a", "human", 1), Some((&vec![1.0; DIM], None, &[])));
        let b = s.apply_run(2, true, &run("git status", 0, "C:/a", "human", 2), Some((&vec![1.0; DIM], Some("git: x".into()), &[one(1.0), one(2.0)])));
        let owners = |s: &Store| {
            let mut o: Vec<u32> = s.desc.pairs().map(|p| p.0).collect();
            o.sort();
            o
        };
        assert_eq!(owners(&s), vec![b, b]);
        s.set_descriptions(&[("eas build", Some("eas: EAS command line tool".into()), vec![one(3.0)]), ("git status", Some("git: y".into()), vec![one(4.0)])]);
        // each entry's old vectors gone, the new ones in; nothing else touched
        assert_eq!(owners(&s), vec![a, b]);
        assert_eq!(s.desc.len(), 2);
        assert_eq!(s.entries[a as usize].desc.as_deref(), Some("eas: EAS command line tool"));
        assert_eq!(s.entries[b as usize].desc.as_deref(), Some("git: y"));
        // an unknown text changes nothing
        s.set_descriptions(&[("never ran", Some("x".into()), vec![one(5.0)])]);
        assert_eq!(s.desc.pairs().count(), 2);
    }

    #[test]
    fn a_description_vector_shared_is_held_once() {
        let mut s = Store::default();
        let v = |x: f32| {
            let mut v = vec![0.0; DIM];
            v[0] = x;
            v[1] = 1.0;
            v
        };
        let a = s.apply_run(1, true, &run("git push", 0, "C:/a", "human", 1), Some((&vec![1.0; DIM], None, &[v(1.0), v(2.0)])));
        let b = s.apply_run(2, true, &run("git pull", 0, "C:/a", "human", 2), Some((&vec![1.0; DIM], None, &[v(1.0), v(3.0)])));
        assert_eq!(s.desc.len(), 3, "the sentence both share is held once");
        assert_eq!(s.desc.pairs().count(), 4);
        let shared = |s: &Store, e: u32| s.desc.pairs().filter(|p| p.0 == e).any(|p| (0.70..0.71).contains(&crate::f16::decode(s.desc.get(p.1)[0])));
        assert!(shared(&s, a) && shared(&s, b));
        // a's vectors replaced: the one only a had goes, the shared one stays for b
        s.set_descriptions(&[("git push", None, vec![v(4.0)])]);
        assert_eq!(s.desc.len(), 3);
        assert!(shared(&s, b) && !shared(&s, a));
        // same scores as one vector per pair
        let q = v(1.0);
        let (sim, _) = crate::search::similarities(&s, &q);
        let best = |e: u32| s.desc.pairs().filter(|p| p.0 == e).map(|p| crate::f16::dot(s.desc.get(p.1), &q)).fold(crate::f16::dot(s.vec(e), &q), f32::max);
        assert_eq!((sim[a as usize], sim[b as usize]), (best(a), best(b)));
    }

    #[test]
    fn the_text_index_finds_every_text_even_when_two_share_a_hash() {
        use std::hash::BuildHasher;
        let mut s = Store::default();
        let a = s.upsert_entry("git status", None, None);
        // a second text whose hash is (made to look like) the first one's
        let h = s.by_text.hasher.hash_one("git stash");
        s.by_text.by_hash.insert(h, a);
        let b = s.upsert_entry("git stash", None, None);
        assert_ne!(a, b);
        assert_eq!(s.entry("git status").map(|x| x.0), Some(a));
        assert_eq!(s.entry("git stash").map(|x| x.0), Some(b));
        assert_eq!(s.upsert_entry("git stash", None, None), b, "found again, not added twice");
        s.by_text.remove(&s.entries, "git stash");
        assert!(!s.knows("git stash"));
        assert!(s.knows("git status"));
        assert!(!s.knows("git stat"));
    }
}

#[cfg(test)]
mod noise_tests {
    use super::looks_like_code;

    #[test]
    fn pasted_code_vs_shell() {
        for c in ["return db_obj", "async def update(", "for i in range(1, n + 1):", ".offset(skip)", "obj_in_data = obj_in",
                  "from sqlalchemy import or_", "6 4 2 1 2 3", "ports:", "result = await db.execute(query)", "};", "print(*arr1)",
                  "build: .", "restart: unless-stopped", "- \"127.0.0.1:8000:8000\"", "image: postgres:16"] {
            assert!(looks_like_code(c), "{c}");
        }
        for c in ["docker-compose up -d", "FOO=1 npm test", "for f in *.py; do echo $f; done", "$x = 1", "d:", "./run.sh",
                  "../x/build.ps1", "python -c \"import x\"", "git commit -m \"fix: y\"", "if (Test-Path x) { rm x }", "import-module posh-git",
                  "d: ", "git -C d:/x status", "echo key: value", "npm run dev -- --port 3000"] {
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
        assert!(!is_self_ref("cd /c/Users/me/reman && cargo build --release"));
        assert!(!is_self_ref("git clone https://github.com/x/remanufacture"));
        assert!(!is_self_ref("docker-compose down"));
    }
}
