//! Ranking. Two modes:
//!  - `Semantic`: exact port of the Python daemon (semantic + small freq/recency tiebreakers,
//!    weak-confidence -> literal word-overlap fallback). Kept for parity checks.
//!  - `Hybrid` (default): semantic + nucleo fuzzy + prefix, so `dock comp up` or a half-typed
//!    command finds the exact thing instantly while natural-language intents still work.
use crate::config::{self, DIM};
use crate::store::{Scope, Store};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use std::collections::HashMap;

/// bge sims run hot (gibberish ~0.70): confidence = decent sim AND literal overlap.
pub const WEAK_SIM: f32 = 0.66;
const STOP: &[&str] = &[
    "the", "that", "this", "thing", "with", "from", "into", "your", "you", "and", "for", "over", "via", "a", "an",
    "of", "to", "in", "on", "my", "it", "is", "all", "some",
];
const W_FUZZY: f32 = 0.35;
const W_FUZZY_NL: f32 = 0.04;
const LONG_FUZZY_CAP: f32 = 0.25;
const W_PREFIX: f32 = 0.05;
const W_HERE: f32 = 0.02;
const W_PIN: f32 = 0.02;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Rank {
    Hybrid,
    Semantic,
}

pub struct Query<'a> {
    pub text: &'a str,
    pub k: usize,
    pub offset: usize,
    pub scope: Scope,
    pub actor: Option<&'a str>,
    pub status: Option<&'a str>,
    pub group: bool,
    pub rank: Rank,
    /// cwd used for a small "run here before" boost when scope is wider than the folder
    pub here: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub idx: u32,
    pub score: f32,
    pub sim: f32,
    pub fuzzy: f32,
    pub variants: u32,
    pub matched_terms: u32,
}

pub struct Outcome {
    /// semantic | hybrid | fuzzy | manual | recent
    pub mode: &'static str,
    pub confident: bool,
    pub hits: Vec<Hit>,
    pub total: usize,
}

#[inline]
pub fn dot(a: &[f32], b: &[f32]) -> f32 {
    // 8 independent accumulators -> the compiler emits packed SIMD without fast-math
    let mut acc = [0f32; 8];
    let (ca, cb) = (a.chunks_exact(8), b.chunks_exact(8));
    for (x, y) in ca.zip(cb) {
        for i in 0..8 {
            acc[i] += x[i] * y[i];
        }
    }
    acc.iter().sum()
}

const FUZZY_SPAN: usize = 160;

/// The positive, literal part of each nucleo atom (negations `!x` can't be prefiltered).
fn fuzzy_atoms(q: &str) -> Vec<String> {
    q.split_whitespace()
        .filter(|a| !a.starts_with('!'))
        .map(|a| a.trim_start_matches(['^', '\'']).trim_end_matches('$').to_lowercase())
        .filter(|a| !a.is_empty() && a.is_ascii())
        .collect()
}

/// Does the query read like the start of a command rather than a description of one? Its first
/// word is (a prefix of) a program that appears in the history, or it has shell-ish punctuation.
pub fn command_like(store: &Store, text: &str) -> bool {
    let Some(first) = text.split_whitespace().next().map(str::to_lowercase) else { return false };
    if text.contains(['-', '/', '\\', '.', '=', ':', '|']) {
        return true;
    }
    // "ssh into the server" starts with a program but reads as a sentence
    let sentence = text.split_whitespace().any(|w| STOP.contains(&w.to_lowercase().as_str()));
    !sentence
        && first.len() >= 2
        && store.entries.iter().any(|e| e.alive && e.gkey.split(' ').next().is_some_and(|p| p.starts_with(first.as_str())))
}

fn is_subsequence(needle: &[u8], hay: &[u8]) -> bool {
    let mut it = hay.iter();
    needle.iter().all(|n| it.any(|h| h == n))
}

pub fn qtokens(q: &str) -> Vec<String> {
    let low = q.to_lowercase();
    low.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| w.len() > 2 && !STOP.contains(w))
        .map(str::to_string)
        .collect()
}

fn freq(runs: u32) -> f32 {
    0.03 * (runs as f32 / (runs as f32 + 20.0))
}

fn recency(last: i64, now: i64) -> f32 {
    0.02 / (1.0 + ((now - last).max(0) as f32 / 86400.0))
}

struct Cand {
    idx: u32,
    runs: u32,
    last: i64,
    here: bool,
}

/// Semantic similarity (max of raw-command and description vectors) for every entry.
pub fn similarities(store: &Store, qv: &[f32]) -> Vec<f32> {
    let n = store.len();
    let mut sim = vec![-1f32; n];
    for (i, e) in store.entries.iter().enumerate() {
        if e.has_vec && e.alive {
            sim[i] = dot(store.vec(i as u32), qv);
        }
    }
    for (j, &owner) in store.desc_owner.iter().enumerate() {
        let d = dot(&store.desc_vecs[j * DIM..(j + 1) * DIM], qv);
        let s = &mut sim[owner as usize];
        if d > *s {
            *s = d;
        }
    }
    sim
}

pub fn search(store: &Store, q: &Query, qv: Option<&[f32]>) -> Outcome {
    let now = config::now();
    let cands: Vec<Cand> = store
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.alive && !e.comment && store.in_scope(e, q.scope))
        .filter_map(|(i, e)| {
            let a = store.agg(e, q.scope);
            store.passes(&a, q.actor, q.status).then(|| Cand {
                idx: i as u32,
                runs: a.runs,
                last: a.last_used,
                here: q.here.is_some_and(|h| e.rows.iter().any(|r| r.cwd == Some(h))),
            })
        })
        .collect();

    let text = q.text.trim();
    if text.is_empty() {
        return recent(store, cands, q);
    }
    let trace = std::env::var_os("REMAN_TRACE").is_some();
    let t0 = std::time::Instant::now();

    // fuzzy (all atoms must match; smart case). nucleo is a DP per haystack and agent-run
    // commands can be multi-KB scripts, so: bitmask prefilter, then subsequence prefilter, and
    // long texts (which contain every letter) only match on literal substrings with a flat score.
    let atoms = fuzzy_atoms(text);
    let qmask = atoms.iter().fold(0u64, |m, a| m | crate::store::char_mask(a));
    let pat = Pattern::parse(text, CaseMatching::Smart, Normalization::Smart);
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut buf = Vec::new();
    let mut fz = vec![0f32; cands.len()];
    let raw_f: Vec<u32> = cands
        .iter()
        .map(|c| {
            let e = &store.entries[c.idx as usize];
            if qmask & !e.mask != 0 {
                return 0;
            }
            if e.lower.len() > FUZZY_SPAN {
                // ~nucleo's per-char score for a contiguous match, without the DP
                return if !atoms.is_empty() && atoms.iter().all(|a| e.lower.contains(a.as_str())) {
                    atoms.iter().map(|a| a.len() as u32 * 16).sum()
                } else {
                    0
                };
            }
            if !atoms.iter().all(|a| is_subsequence(a.as_bytes(), e.lower.as_bytes())) {
                return 0;
            }
            pat.score(Utf32Str::new(&e.text, &mut buf), &mut matcher).unwrap_or(0)
        })
        .collect();
    let fmax = raw_f.iter().copied().max().unwrap_or(0);
    if fmax > 0 {
        for ((f, r), c) in fz.iter_mut().zip(&raw_f).zip(&cands) {
            *f = *r as f32 / fmax as f32;
            // a long script that merely mentions the words is not what you're typing
            if store.entries[c.idx as usize].lower.len() > FUZZY_SPAN {
                *f = f.min(LONG_FUZZY_CAP);
            }
        }
    }
    // command-like queries (`dock comp up`, `npm e2e`) lean on fuzzy; natural-language intents
    // (`seed the database`) lean on meaning and only get a small literal nudge
    let w_fuzzy = if command_like(store, text) { W_FUZZY } else { W_FUZZY_NL };
    if trace {
        crate::daemon::log(&format!("  cands {} fuzzy {:?}", cands.len(), t0.elapsed()));
    }
    let tlow = text.to_lowercase();
    let prefix = |i: u32| store.entries[i as usize].lower.starts_with(&tlow) as u8 as f32;

    let t1 = std::time::Instant::now();
    let sim_all = qv.map(|v| similarities(store, v));
    if trace {
        crate::daemon::log(&format!("  sims {:?}", t1.elapsed()));
    }
    let sim = |i: u32| sim_all.as_ref().map(|s| s[i as usize]).unwrap_or(-1.0);

    // confidence exactly as the Python daemon: best sim >= WEAK_SIM and a literal content-word hit
    let toks = qtokens(text);
    let mut confident = false;
    if sim_all.is_some() {
        let mut order: Vec<usize> = (0..cands.len()).collect();
        let base = |k: usize| sim(cands[k].idx) + freq(cands[k].runs) + recency(cands[k].last, now);
        order.sort_by(|&a, &b| base(b).total_cmp(&base(a)));
        if let Some(&top) = order.first() {
            let best = sim(cands[top].idx);
            let lit = toks.is_empty()
                || order.iter().take(8).any(|&k| {
                    let e = &store.entries[cands[k].idx as usize];
                    let blob = format!("{} {}", e.lower, e.desc.as_deref().unwrap_or("").to_lowercase());
                    toks.iter().any(|w| blob.contains(w.as_str()))
                });
            confident = best >= WEAK_SIM && lit;
        }
    }

    let short = text.chars().count() < 3 || sim_all.is_none();
    let mut scored: Vec<Hit> = Vec::with_capacity(cands.len());
    let mode: &'static str;
    match q.rank {
        Rank::Semantic if sim_all.is_some() && confident => {
            mode = "semantic";
            for c in &cands {
                let s = sim(c.idx);
                scored.push(hit(c.idx, s + freq(c.runs) + recency(c.last, now), s, 0.0));
            }
        }
        Rank::Hybrid if !short && confident => {
            mode = "hybrid";
            for (k, c) in cands.iter().enumerate() {
                let s = sim(c.idx);
                let e = &store.entries[c.idx as usize];
                let score = s
                    + w_fuzzy * fz[k]
                    + W_PREFIX * prefix(c.idx)
                    + freq(c.runs)
                    + recency(c.last, now)
                    + if c.here { W_HERE } else { 0.0 }
                    + if e.pinned { W_PIN } else { 0.0 };
                scored.push(hit(c.idx, score, s, fz[k]));
            }
        }
        Rank::Hybrid if fmax > 0 => {
            mode = "fuzzy";
            for (k, c) in cands.iter().enumerate().filter(|(k, _)| fz[*k] > 0.0) {
                let s = sim(c.idx).max(0.0);
                let score = fz[k] + W_PREFIX * prefix(c.idx) + 0.25 * s + freq(c.runs) + recency(c.last, now)
                    + if c.here { W_HERE } else { 0.0 };
                scored.push(hit(c.idx, score, sim(c.idx), fz[k]));
            }
        }
        _ => {
            // literal word-overlap over the real history (the user's "manual" rule). Invents nothing.
            mode = "manual";
            let words: Vec<String> = text.split_whitespace().filter(|w| w.len() > 1).map(|w| w.to_lowercase()).collect();
            for c in &cands {
                let low = &store.entries[c.idx as usize].lower;
                let m = words.iter().filter(|w| low.contains(w.as_str())).count() as u32;
                if m > 0 {
                    let mut h = hit(c.idx, m as f32 * 1000.0 + c.runs.min(999) as f32, sim(c.idx), 0.0);
                    h.matched_terms = m;
                    scored.push(h);
                }
            }
        }
    }
    scored.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.idx.cmp(&b.idx)));
    let group = q.group && mode != "manual";
    finish(store, scored, q, group, mode, confident)
}

fn hit(idx: u32, score: f32, sim: f32, fuzzy: f32) -> Hit {
    Hit { idx, score, sim, fuzzy, variants: 1, matched_terms: 0 }
}

fn finish(store: &Store, scored: Vec<Hit>, q: &Query, group: bool, mode: &'static str, confident: bool) -> Outcome {
    let hits = if group {
        let mut sizes: HashMap<&str, u32> = HashMap::new();
        for h in &scored {
            *sizes.entry(store.entries[h.idx as usize].gkey.as_str()).or_default() += 1;
        }
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for mut h in scored {
            let gk = store.entries[h.idx as usize].gkey.as_str();
            if seen.insert(gk) {
                h.variants = sizes[gk];
                out.push(h);
            }
        }
        out
    } else {
        scored
    };
    let total = hits.len();
    let hits = hits.into_iter().skip(q.offset).take(if q.k == 0 { usize::MAX } else { q.k }).collect();
    Outcome { mode, confident, hits, total }
}

/// Browse: newest first (pinned float to the top), no embedding at all.
fn recent(store: &Store, mut cands: Vec<Cand>, q: &Query) -> Outcome {
    cands.sort_by(|a, b| {
        let pa = store.entries[a.idx as usize].pinned;
        let pb = store.entries[b.idx as usize].pinned;
        pb.cmp(&pa).then(b.last.cmp(&a.last)).then(b.runs.cmp(&a.runs))
    });
    let scored = cands.into_iter().map(|c| hit(c.idx, 0.0, 0.0, 0.0)).collect();
    finish(store, scored, q, false, "recent", true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{self, Run};
    use rusqlite::Connection;

    fn store_with(cmds: &[&str]) -> Store {
        let db = Connection::open_in_memory().unwrap();
        db::migrate(&db).unwrap();
        let mut s = Store::default();
        for (i, c) in cmds.iter().enumerate() {
            let r = Run { cmd: c.to_string(), exit: Some(0), cwd: Some("C:/p".into()), actor: "human".into(), ts: 100 + i as i64, ..Default::default() };
            let rec = db::record_run(&db, &r).unwrap();
            let mut v = vec![0.0; DIM];
            v[i % DIM] = 1.0;
            s.apply_run(rec.command_id, rec.new_row, &r, Some((&v, None, &[])));
        }
        s
    }

    fn q(text: &str) -> Query<'_> {
        Query { text, k: 5, offset: 0, scope: Scope::All, actor: None, status: None, group: false, rank: Rank::Hybrid, here: None }
    }

    #[test]
    fn fuzzy_finds_abbreviated_command() {
        let s = store_with(&["docker compose up -d", "git status", "docker ps", "npm run dev"]);
        let out = search(&s, &q("dock comp up"), None);
        assert_eq!(out.mode, "fuzzy");
        assert_eq!(s.entries[out.hits[0].idx as usize].text, "docker compose up -d");
    }

    #[test]
    fn empty_query_browses_newest_first() {
        let s = store_with(&["a1", "b2", "c3"]);
        let out = search(&s, &q(""), None);
        let texts: Vec<&str> = out.hits.iter().map(|h| s.entries[h.idx as usize].text.as_str()).collect();
        assert_eq!(texts, ["c3", "b2", "a1"]);
    }

    #[test]
    fn manual_fallback_counts_terms() {
        let s = store_with(&["kubectl get pods", "git log"]);
        let out = search(&s, &q("zzqq pods kubectl"), None);
        assert_eq!(out.mode, "manual");
        assert_eq!(out.hits[0].matched_terms, 2);
    }

    #[test]
    fn command_like_queries() {
        let s = store_with(&["docker compose up -d", "npm run dev", "git status"]);
        assert!(command_like(&s, "dock comp up"));
        assert!(command_like(&s, "npm e2e"));
        assert!(command_like(&s, "cd ../x"));
        assert!(!command_like(&s, "seed the database"));
        assert!(!command_like(&s, "tear down containers"));
        assert!(!command_like(&s, "git into the repo"));
    }

    #[test]
    fn dot_matches_naive() {
        let a: Vec<f32> = (0..DIM).map(|i| i as f32 * 0.01).collect();
        let b: Vec<f32> = (0..DIM).map(|i| 1.0 - i as f32 * 0.001).collect();
        let naive: f32 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
        assert!((dot(&a, &b) - naive).abs() < 1e-2);
    }
}
