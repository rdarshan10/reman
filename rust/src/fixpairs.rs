//! Fix-pairs, now LIVE: a failure is remembered only once a real fix follows it (same session
//! or folder). Certainty ladder from the Python phase-4 spike:
//!   exact            - the same command failed, then later succeeded
//!   shell_correction - the very next command is a near-identical edit (typo fix)
//!   inferred         - same base program within 10 commands
//! Unpaired failures evaporate (never stored).
use crate::dym::norm_edit;
use crate::store::Store;
use anyhow::Result;
use rusqlite::{Connection, params};
use std::collections::HashMap;

pub const WINDOW: u64 = 10;

pub fn base(cmd: &str) -> String {
    let t: Vec<&str> = cmd.split_whitespace().collect();
    match t.as_slice() {
        [] => String::new(),
        [a, b, ..] if !b.starts_with('-') => format!("{a} {b}"),
        [a, ..] => a.to_string(),
    }
}

pub fn classify(failed: &str, fixed: &str, gap: u64) -> Option<&'static str> {
    if failed == fixed {
        return Some("exact");
    }
    if gap == 1 && norm_edit(failed, fixed) <= 0.15 {
        return Some("shell_correction");
    }
    if gap <= WINDOW && base(failed) == base(fixed) {
        return Some("inferred");
    }
    None
}

fn tier(c: &str) -> u8 {
    match c {
        "exact" => 3,
        "shell_correction" => 2,
        _ => 1,
    }
}

#[derive(Debug, Clone)]
pub struct Pair {
    pub failed: String,
    pub fixed: String,
    pub confidence: &'static str,
}

#[derive(Debug, Clone)]
pub struct PairRec {
    pub fixed: String,
    pub confidence: String,
    pub count: u32,
    pub last_seen: i64,
}

struct Ev {
    pos: u64,
    cmd: String,
}

/// Per-stream failure buffers (stream = session, else folder).
#[derive(Default)]
pub struct Tracker {
    buffers: HashMap<String, Vec<Ev>>,
    pos: HashMap<String, u64>,
    pub pairs: HashMap<String, Vec<PairRec>>,
}

fn norm_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl Tracker {
    pub fn load(db: &Connection) -> Result<Self> {
        let mut t = Tracker::default();
        let mut st = db.prepare("SELECT failed_text, fixed_cmd, confidence, count, last_seen FROM fix_pairs")?;
        let rows = st.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, PairRec { fixed: r.get(1)?, confidence: r.get(2)?, count: r.get::<_, i64>(3)? as u32, last_seen: r.get::<_, Option<i64>>(4)?.unwrap_or(0) }))
        })?;
        for r in rows {
            let (f, p) = r?;
            t.pairs.entry(norm_ws(&f)).or_default().push(p);
        }
        Ok(t)
    }

    /// Feed one run (exit None/negative = unknown -> ignored). Returns newly proven pairs.
    pub fn observe(&mut self, stream: &str, cmd: &str, exit: Option<i64>) -> Vec<Pair> {
        let Some(exit) = exit.filter(|e| *e >= 0) else { return vec![] };
        let p = self.pos.entry(stream.to_string()).or_default();
        *p += 1;
        let pos = *p;
        let buf = self.buffers.entry(stream.to_string()).or_default();
        if exit != 0 {
            buf.push(Ev { pos, cmd: cmd.to_string() });
            let min = pos.saturating_sub(WINDOW);
            buf.retain(|e| e.pos >= min);
            return vec![];
        }
        // success: resolve the newest matching failure (one per success, as the spike did)
        let mut out = Vec::new();
        if let Some(i) = (0..buf.len()).rev().find(|&i| classify(&buf[i].cmd, cmd, pos - buf[i].pos).is_some()) {
            let f = buf.remove(i);
            let conf = classify(&f.cmd, cmd, pos - f.pos).unwrap();
            out.push(Pair { failed: f.cmd, fixed: cmd.to_string(), confidence: conf });
        }
        out
    }

    pub fn persist(&mut self, db: &Connection, p: &Pair, cwd: Option<&str>, ts: i64) -> Result<()> {
        db.execute(
            "INSERT INTO fix_pairs (failed_text, fixed_cmd, confidence, cwd, count, first_seen, last_seen)
             VALUES (?,?,?,?,1,?,?)
             ON CONFLICT(failed_text, fixed_cmd) DO UPDATE SET count=count+1, last_seen=excluded.last_seen,
               confidence=CASE WHEN excluded.confidence='exact' OR confidence='inferred' THEN excluded.confidence ELSE confidence END",
            params![p.failed, p.fixed, p.confidence, cwd, ts, ts],
        )?;
        let v = self.pairs.entry(norm_ws(&p.failed)).or_default();
        match v.iter_mut().find(|r| r.fixed == p.fixed) {
            Some(r) => {
                r.count += 1;
                r.last_seen = ts;
                if tier(p.confidence) > tier(&r.confidence) {
                    r.confidence = p.confidence.to_string();
                }
            }
            None => v.push(PairRec { fixed: p.fixed.clone(), confidence: p.confidence.to_string(), count: 1, last_seen: ts }),
        }
        Ok(())
    }

    /// Proven fixes for a failed command, best first (tier, then count, then recency).
    pub fn lookup(&self, failed: &str, store: &Store) -> Vec<PairRec> {
        let mut v: Vec<PairRec> = self
            .pairs
            .get(&norm_ws(failed))
            .map(|v| v.iter().filter(|p| store.knows(&p.fixed) && p.fixed != failed.trim()).cloned().collect())
            .unwrap_or_default();
        v.sort_by(|a, b| (tier(&b.confidence), b.count, b.last_seen).cmp(&(tier(&a.confidence), a.count, a.last_seen)));
        v
    }
}

/// Stream key for live tracking: the session id when the shell provides one, else the folder.
pub fn stream_key(session: &str, cwd: Option<&str>) -> String {
    if !session.is_empty() { format!("s:{session}") } else { format!("d:{}", crate::config::norm_path(cwd.unwrap_or(""))) }
}

/// Backfill from the execution history (streams split on 5-minute gaps). Returns pairs found.
pub fn rebuild(store: &Store) -> Vec<(Pair, Option<String>, i64)> {
    let mut t = Tracker::default();
    let mut last: HashMap<String, i64> = HashMap::new();
    let mut out = Vec::new();
    for x in &store.execs {
        let e = &store.entries[x.entry as usize];
        if e.comment {
            continue;
        }
        let cwd = x.cwd.map(|c| store.cwd_name(c).to_string());
        let key = stream_key(store.session_name(x.session), cwd.as_deref());
        if last.get(&key).is_some_and(|l| x.ts - l > 300) {
            t.buffers.remove(&key);
        }
        last.insert(key.clone(), x.ts);
        for p in t.observe(&key, &e.text, x.exit) {
            out.push((p, cwd.clone(), x.ts));
        }
    }
    out
}

/// What a fix changed, word by word, when it is a variant of the command that failed:
/// `adds --build`, `drops -v`, `ci → install --legacy-peer-deps`, `gti → git`. None when the two
/// share too little to be variants (a different command altogether) or are the same.
pub fn diff(failed: &str, fixed: &str) -> Option<String> {
    let (a, b): (Vec<&str>, Vec<&str>) = (failed.split_whitespace().collect(), fixed.split_whitespace().collect());
    // longest common subsequence of words
    let (n, m) = (a.len(), b.len());
    let mut l = vec![vec![0u16; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            l[i][j] = if a[i] == b[j] { l[i + 1][j + 1] + 1 } else { l[i + 1][j].max(l[i][j + 1]) };
        }
    }
    let common = l[0][0] as usize;
    // the same program, or at least half the words shared; else it's another command
    if common == 0 || common == n.max(m) || (a[0] != b[0] && common * 2 < n.max(m)) {
        return None;
    }
    let (mut i, mut j, mut gone, mut added) = (0, 0, Vec::new(), Vec::new());
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            i += 1;
            j += 1;
        } else if j < m && (i == n || l[i][j + 1] >= l[i + 1][j]) {
            added.push(b[j]);
            j += 1;
        } else {
            gone.push(a[i]);
            i += 1;
        }
    }
    let s = match (gone.is_empty(), added.is_empty()) {
        (true, false) => format!("adds {}", added.join(" ")),
        (false, true) => format!("drops {}", gone.join(" ")),
        _ => format!("{} → {}", gone.join(" "), added.join(" ")),
    };
    (s.chars().count() <= 60).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_a_fix_changed() {
        assert_eq!(diff("docker compose up", "docker compose up --build").as_deref(), Some("adds --build"));
        assert_eq!(diff("npm ci --verbose", "npm ci").as_deref(), Some("drops --verbose"));
        assert_eq!(diff("npm ci", "npm install --legacy-peer-deps").as_deref(), Some("ci → install --legacy-peer-deps"));
        assert_eq!(diff("gti status", "git status").as_deref(), Some("gti → git"));
        assert_eq!(diff("pip install nummpy", "pip install numpy").as_deref(), Some("nummpy → numpy"));
        assert_eq!(diff("python app.py", "python app.py"), None, "the same command, retried");
        assert_eq!(diff("docker compose pull", "make up"), None, "another command altogether");
        assert_eq!(diff("cargo build", "npm run build"), None, "only one word in common, and not the program");
    }

    /// Port of reman_fixpairs.simulate(): each tier detected, unpaired failure evaporates.
    #[test]
    fn simulate_ac() {
        let s = [
            ("pip install nummpy", 1),
            ("pip install numpy", 0),
            ("python app.py", 1),
            ("pip install -r requirements.txt", 0),
            ("python app.py", 0),
            ("docker compose pull", 1),
            ("docker compose up -d", 0),
            ("terraform apply", 1),
        ];
        let mut t = Tracker::default();
        let mut got = Vec::new();
        for (c, e) in s {
            got.extend(t.observe("s", c, Some(e)));
        }
        let g: Vec<(&str, &str, &str)> = got.iter().map(|p| (p.failed.as_str(), p.fixed.as_str(), p.confidence)).collect();
        assert_eq!(
            g,
            vec![
                ("pip install nummpy", "pip install numpy", "shell_correction"),
                ("python app.py", "python app.py", "exact"),
                ("docker compose pull", "docker compose up -d", "inferred"),
            ]
        );
        assert!(got.iter().all(|p| p.failed != "terraform apply"));
    }

    #[test]
    fn unknown_exit_ignored() {
        let mut t = Tracker::default();
        assert!(t.observe("s", "gti status", None).is_empty());
        assert!(t.observe("s", "git status", Some(0)).is_empty());
    }
}
