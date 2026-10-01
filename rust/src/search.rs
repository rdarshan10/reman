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

/// bge sims run hot: an unrelated command scores 0.55-0.75 against a sentence, a real match
/// 0.65-0.90, so similarity alone can't say "found it". A hit is CLOSE (worth offering as the
/// answer) when it is similar enough on its own, or fairly similar AND has the query's words
/// (weighted by rarity, in the command or its description): sim + 0.2 x words >= 0.788.
/// Calibrated with `e2e/search_quality.py --dump` on its sample history and on a real one:
/// intents never run reach 0.739 alone and 0.772 blended (`npm outdated` for "update homebrew
/// packages"); real matches blend 0.804 and up.
pub const CLOSE_ALONE: f32 = 0.75;
pub const CLOSE_WITH_WORDS: f32 = 0.66;
const CLOSE_BLEND: f32 = 0.788;
const W_CLOSE_WORDS: f32 = 0.2;
/// a command reman can't describe is raw text only: it needs this share of the query's words
const UNDESCRIBED_WORDS: f32 = 0.35;
/// fuzzy share of a perfect match that counts as "the typed text is this command"
pub const CLOSE_FUZZY: f32 = 0.5;
const STOP: &[&str] = &[
    "the", "that", "this", "thing", "with", "from", "into", "your", "you", "and", "for", "over", "via", "a", "an",
    "of", "to", "in", "on", "my", "it", "is", "all", "some",
];
/// also not content, but common in commands too (`docker compose up`): kept out of STOP, which
/// decides whether a query reads as a sentence
const FILLER: &[&str] = &[
    "these", "those", "every", "whatever", "what", "who", "when", "where", "which", "how", "does", "was", "were",
    "are", "here", "there", "again", "just", "without", "up", "do", "be", "as", "at", "by", "or", "me", "we", "so",
    "if", "no", "can", "want", "need", "please",
];
/// ranking nudge per share of the query's words found in the command or its description
const W_WORDS: f32 = 0.05;
const W_FUZZY: f32 = 0.35;
const W_FUZZY_NL: f32 = 0.04;
const LONG_FUZZY_CAP: f32 = 0.25;
const W_PREFIX: f32 = 0.05;
const W_HERE: f32 = 0.02;
const W_PIN: f32 = 0.02;
const W_SCRIPT: f32 = 0.04;
/// an agent's one-off (ran once, never by you) - usually exploration: `cd x; echo ===; grep ...`
const W_ONEOFF: f32 = 0.03;
const W_FAILING: f32 = 0.06;
/// below this share of a perfect match, a fuzzy hit is letters scattered by chance
const FUZZY_FLOOR: f32 = 0.3;

/// Bounded nudge against multi-KB / multi-line scripts (mostly agent-run) that merely mention the
/// query's words: you want the command you'd rerun, not a 40-line heredoc. 0 up to 160 chars,
/// linear to W_SCRIPT at 1000+, plus a little for multi-line text.
fn script_penalty(e: &crate::store::Entry) -> f32 {
    let len = e.text.len() as f32;
    let long = ((len - 160.0) / 840.0).clamp(0.0, 1.0) * W_SCRIPT;
    long + if e.text.contains('\n') { 0.01 } else { 0.0 }
}

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
    /// [since, until): only commands that ran in this window (timewords.rs)
    pub window: Option<(i64, i64)>,
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub idx: u32,
    pub score: f32,
    pub sim: f32,
    pub fuzzy: f32,
    pub variants: u32,
    pub matched_terms: u32,
    /// clearly what was asked for (see CLOSE_ALONE); the rest are only loosely related
    pub close: bool,
    /// share of the query's words (weighted by rarity) in the command or its description
    pub words: f32,
}

pub struct Outcome {
    /// semantic | hybrid | fuzzy | manual | recent
    pub mode: &'static str,
    /// the best hit is close: false means "nothing like this in the history"
    pub confident: bool,
    pub hits: Vec<Hit>,
    pub total: usize,
    /// a folded hit -> every variant it stands for, itself first, best first
    pub members: HashMap<u32, Vec<u32>>,
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

/// The query's content words: 2+ letters (`ps`, `qr` and `db` say a lot), stop words out.
fn content_words(q: &str) -> Vec<String> {
    q.to_lowercase().split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| w.len() >= 2 && !STOP.contains(w) && !FILLER.contains(w)).map(String::from).collect()
}

/// `stem` at the start of a word of `text`: "start" is in "start-server", not in "restart".
fn has_word(text: &str, stem: &str) -> bool {
    text.match_indices(stem).any(|(i, _)| i == 0 || !text.as_bytes()[i - 1].is_ascii_alphanumeric())
}

/// Crude stem for the word-overlap check: "migrations" / "migration" / "migrate" all share
/// "migrat", "lines" and "line" share "line". Short words stay whole.
fn stem(w: &str) -> String {
    let w = if w.len() > 4 && w.ends_with('s') && !w.ends_with("ss") { &w[..w.len() - 1] } else { w };
    if w.len() < 6 {
        return w.to_string();
    }
    let keep = (w.len() * 2 / 3).max(5);
    w[..keep].to_string()
}

/// Edit distance of at most one (a substitution, insertion, deletion or swap of neighbours):
/// enough to tell `dcoker` from `docker`. Linear, no allocation - it runs over the vocabulary.
fn osa_within_one(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (s, l) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if l.len() - s.len() > 1 {
        return false;
    }
    let Some(i) = s.iter().zip(l).position(|(x, y)| x != y) else { return true };
    if s.len() == l.len() {
        s[i + 1..] == l[i + 1..] || (i + 1 < s.len() && s[i] == l[i + 1] && s[i + 1] == l[i] && s[i + 2..] == l[i + 2..])
    } else {
        s[i..] == l[i + 1..]
    }
}

/// The query's content words, as stems. A word found nowhere in the history is probably a typo:
/// it is swapped for a history word one edit away (`dcoker` -> `docker`), if there is one.
fn query_words(text: &str, blobs: &[&str]) -> Vec<String> {
    content_words(text)
        .into_iter()
        .map(|w| {
            let s = stem(&w);
            if w.len() < 5 || blobs.iter().any(|b| has_word(b, &s)) {
                return s;
            }
            blobs.iter().flat_map(|b| b.split(|c: char| !c.is_ascii_alphanumeric())).find(|x| x.len() >= 4 && osa_within_one(&w, x)).map(stem).unwrap_or(s)
        })
        .collect()
}

/// See CLOSE_ALONE. For a command reman can't describe, similarity rests on raw text alone and
/// is unreliable (an unknown tool's name can score 0.75 against an unrelated request), so it needs more of
/// the query's words. An agent's one-off is exploration, never the answer: its echo lines are
/// full of words.
pub fn is_close(sim: f32, words: f32, fuzzy: f32, cmd_like: bool, described: bool, oneoff: bool) -> bool {
    let enough_words = if described { words > 0.0 } else { words >= UNDESCRIBED_WORDS };
    !oneoff
        && ((sim >= CLOSE_ALONE && (described || enough_words))
            || (sim >= CLOSE_WITH_WORDS && enough_words && sim + W_CLOSE_WORDS * words >= CLOSE_BLEND)
            || (cmd_like && fuzzy >= CLOSE_FUZZY))
}

/// The tools a query names (`start the astro dev server` names astro): words that are programs
/// some command of yours runs, and not everyday words in other tools' descriptions (`start`,
/// `find`, `python` are, so they name nothing). A command that runs none of them is for some
/// other tool, however alike the sentence: never close.
fn named_tools(store: &Store, text: &str) -> Vec<String> {
    content_words(text)
        .into_iter()
        .filter(|w| w.len() >= 3 && store.programs.contains(w.as_str()))
        .filter(|w| {
            let elsewhere = |e: &&crate::store::Entry| e.alive && !e.progs.iter().any(|p| p == w) && e.desc_words.as_deref().is_some_and(|d| has_word(d, w));
            store.entries.iter().filter(elsewhere).take(2).count() < 2
        })
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
    oneoff: bool,
    /// only ever failed: `gti status` must not outrank `git status` for `git st`
    failing: bool,
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
    let show_self = q.text.to_lowercase().contains("reman");
    // a time named in the query (`deploy last week`): only commands that ran then, there
    let ran_then: Option<std::collections::HashSet<u32>> = q.window.map(|(since, until)| {
        store.execs.iter().filter(|x| x.ts >= since && x.ts < until && store.cwd_in_scope(x.cwd, q.scope)).map(|x| x.entry).collect()
    });
    let cands: Vec<Cand> = store
        .entries
        .iter()
        .enumerate()
        .filter(|(i, e)| e.recallable(show_self) && store.in_scope(e, q.scope) && ran_then.as_ref().is_none_or(|s| s.contains(&(*i as u32))))
        .filter_map(|(i, e)| {
            let a = store.agg(e, q.scope);
            store.passes(&a, q.actor, q.status).then(|| Cand {
                idx: i as u32,
                runs: a.runs,
                last: a.last_used,
                here: q.here.is_some_and(|h| e.rows.iter().any(|r| r.cwd == Some(h))),
                oneoff: a.human == 0 && a.agent > 0 && a.runs <= 1,
                failing: a.ok == 0 && a.fail > 0,
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
    // normalise against the query matched against itself (the best any text can do), not
    // against the best candidate: otherwise, when nothing really matches, letters scattered
    // across an unrelated command get stretched up to ~1.0
    let ideal = pat.score(Utf32Str::new(text, &mut buf), &mut matcher).unwrap_or(1).max(1);
    let mut fmax = 0;
    for ((f, r), c) in fz.iter_mut().zip(&raw_f).zip(&cands) {
        let n = (*r as f32 / ideal as f32).min(1.0);
        *f = if n < FUZZY_FLOOR { 0.0 } else { n };
        // a long script that merely mentions the words is not what you're typing
        if store.entries[c.idx as usize].lower.len() > FUZZY_SPAN {
            *f = f.min(LONG_FUZZY_CAP);
        }
        if *f > 0.0 {
            fmax = fmax.max(*r);
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

    // the query's words, and which of them each candidate's command or description contains.
    // Long scripts are left out: never the answer, and their echo lines are full of words.
    let short_text = |c: &Cand| store.entries[c.idx as usize].lower.len() <= FUZZY_SPAN;
    let texts: Vec<&str> = cands
        .iter()
        .filter(|c| short_text(c))
        .flat_map(|c| {
            let e = &store.entries[c.idx as usize];
            std::iter::once(e.lower.as_str()).chain(e.desc_words.as_deref())
        })
        .collect();
    let toks = query_words(text, &texts);
    let words: Vec<f32> = {
        // which of the query's words each candidate has, then each word weighted by how rare it
        // is here (idf): "undo" in one description says more than "commit" in dozens, and a word
        // found nowhere ("homebrew") weighs most, so missing it counts most
        let has: Vec<Vec<bool>> = cands
            .iter()
            .map(|c| {
                let e = &store.entries[c.idx as usize];
                if !short_text(c) {
                    return vec![false; toks.len()];
                }
                toks.iter().map(|t| has_word(&e.lower, t) || e.desc_words.as_deref().is_some_and(|d| has_word(d, t))).collect()
            })
            .collect();
        let n = cands.len() as f32;
        let weight: Vec<f32> = (0..toks.len()).map(|j| ((n + 1.0) / (has.iter().filter(|h| h[j]).count() as f32 + 1.0)).ln().max(0.1)).collect();
        let total: f32 = weight.iter().sum();
        has.iter().map(|h| if total > 0.0 { h.iter().zip(&weight).filter(|(x, _)| **x).map(|(_, w)| w).sum::<f32>() / total } else { 0.0 }).collect()
    };
    let cmd_like = w_fuzzy == W_FUZZY;
    let named = named_tools(store, text);
    let for_named = |c: &Cand| {
        let e = &store.entries[c.idx as usize];
        named.iter().all(|t| has_word(&e.lower, t) || e.desc_words.as_deref().is_some_and(|d| has_word(d, t)))
    };

    let short = text.chars().count() < 3 || sim_all.is_none();
    let mut scored: Vec<Hit> = Vec::with_capacity(cands.len());
    let mode: &'static str;
    match q.rank {
        Rank::Semantic if !short => {
            mode = "semantic";
            for (k, c) in cands.iter().enumerate() {
                let s = sim(c.idx);
                let mut h = hit(c.idx, s + freq(c.runs) + recency(c.last, now), s, 0.0);
                h.close = for_named(c) && is_close(s, words[k], 0.0, false, store.entries[c.idx as usize].desc.is_some(), c.oneoff);
                h.words = words[k];
                scored.push(h);
            }
        }
        Rank::Hybrid if !short => {
            mode = "hybrid";
            for (k, c) in cands.iter().enumerate() {
                let s = sim(c.idx);
                let e = &store.entries[c.idx as usize];
                let score = s
                    + w_fuzzy * fz[k]
                    + W_WORDS * words[k]
                    + W_PREFIX * prefix(c.idx)
                    + freq(c.runs)
                    + recency(c.last, now)
                    + if c.here { W_HERE } else { 0.0 }
                    + if e.pinned { W_PIN } else { 0.0 }
                    - if c.oneoff { W_ONEOFF } else { 0.0 }
                    - if c.failing { W_FAILING } else { 0.0 }
                    - script_penalty(e);
                let mut h = hit(c.idx, score, s, fz[k]);
                h.close = for_named(c) && is_close(s, words[k], fz[k], cmd_like, e.desc.is_some(), c.oneoff);
                h.words = words[k];
                scored.push(h);
            }
        }
        _ if fmax > 0 => {
            // too short to embed: the typed text is all there is
            mode = "fuzzy";
            for (k, c) in cands.iter().enumerate().filter(|(k, _)| fz[*k] > 0.0) {
                let s = sim(c.idx).max(0.0);
                let score = fz[k] + W_PREFIX * prefix(c.idx) + 0.25 * s + freq(c.runs) + recency(c.last, now)
                    + if c.here { W_HERE } else { 0.0 }
                    - if c.failing { W_FAILING } else { 0.0 };
                let mut h = hit(c.idx, score, sim(c.idx), fz[k]);
                h.close = fz[k] >= CLOSE_FUZZY;
                scored.push(h);
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
                    h.close = m as usize == words.len();
                    scored.push(h);
                }
            }
        }
    }
    // answers first: a close hit always outranks one that isn't (an agent's one-off that quotes
    // the query word for word stays below the command it was about)
    scored.sort_by(|a, b| b.close.cmp(&a.close).then(b.score.total_cmp(&a.score)).then_with(|| a.idx.cmp(&b.idx)));
    let group = q.group && mode != "manual";
    finish(store, scored, q, group, mode)
}

fn hit(idx: u32, score: f32, sim: f32, fuzzy: f32) -> Hit {
    Hit { idx, score, sim, fuzzy, variants: 1, matched_terms: 0, close: false, words: 0.0 }
}

fn finish(store: &Store, scored: Vec<Hit>, q: &Query, group: bool, mode: &'static str) -> Outcome {
    let confident = scored.first().is_some_and(|h| h.close);
    let mut members: HashMap<u32, Vec<u32>> = HashMap::new();
    let hits = if group {
        let mut by_shape: HashMap<&str, Vec<u32>> = HashMap::new();
        for h in &scored {
            by_shape.entry(store.entries[h.idx as usize].shape.as_str()).or_default().push(h.idx);
        }
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for mut h in scored {
            let gk = store.entries[h.idx as usize].shape.as_str();
            if seen.insert(gk) {
                h.variants = by_shape[gk].len() as u32;
                if h.variants > 1 {
                    members.insert(h.idx, by_shape[gk].clone());
                }
                out.push(h);
            }
        }
        out
    } else {
        scored
    };
    let total = hits.len();
    let hits = hits.into_iter().skip(q.offset).take(if q.k == 0 { usize::MAX } else { q.k }).collect();
    Outcome { mode, confident, hits, total, members }
}

/// Browse: newest first (pinned float to the top), no embedding at all.
fn recent(store: &Store, mut cands: Vec<Cand>, q: &Query) -> Outcome {
    cands.sort_by(|a, b| {
        let pa = store.entries[a.idx as usize].pinned;
        let pb = store.entries[b.idx as usize].pinned;
        pb.cmp(&pa).then(b.last.cmp(&a.last)).then(b.runs.cmp(&a.runs))
    });
    let scored = cands.into_iter().map(|c| Hit { close: true, ..hit(c.idx, 0.0, 0.0, 0.0) }).collect();
    let mut out = finish(store, scored, q, false, "recent");
    out.confident = true;
    out
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
        Query { text, k: 5, offset: 0, scope: Scope::All, actor: None, status: None, group: false, rank: Rank::Hybrid, here: None, window: None }
    }

    #[test]
    fn fuzzy_finds_abbreviated_command() {
        let s = store_with(&["docker compose up -d", "git status", "docker ps", "npm run dev"]);
        let out = search(&s, &q("dock comp up"), None);
        assert_eq!(out.mode, "fuzzy");
        assert_eq!(s.entries[out.hits[0].idx as usize].text, "docker compose up -d");
    }

    #[test]
    fn a_command_that_only_failed_never_leads() {
        let mut s = store_with(&["git status"]);
        // `gti status` failed three times, more recently and more often than `git status` ran
        for ts in 200..203 {
            let r = Run { cmd: "gti status".into(), exit: Some(1), cwd: Some("C:/p".into()), actor: "human".into(), ts, ..Default::default() };
            let v = vec![0.0; DIM];
            s.apply_run(1200, ts == 200, &r, Some((&v, None, &[])));
        }
        let out = search(&s, &q("git st"), None);
        assert_eq!(s.entries[out.hits[0].idx as usize].text, "git status");
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
    fn one_edit_apart() {
        for (a, b) in [("dcoker", "docker"), ("docker", "docker"), ("dockr", "docker"), ("dockerr", "docker"), ("docket", "docker")] {
            assert!(osa_within_one(a, b), "{a} ~ {b}");
        }
        for (a, b) in [("dkocre", "docker"), ("dock", "docker"), ("kubectl", "docker")] {
            assert!(!osa_within_one(a, b), "{a} !~ {b}");
        }
    }

    #[test]
    fn words_match_at_word_starts() {
        assert!(has_word("kubectl rollout restart", "rollout"));
        assert!(has_word("docker compose build --no-cache", "cache"));
        assert!(!has_word("kubectl rollout restart", "start"));
        assert_eq!(stem("lines"), "line");
        assert_eq!(stem("containers"), "contai");
    }

    #[test]
    fn dot_matches_naive() {
        let a: Vec<f32> = (0..DIM).map(|i| i as f32 * 0.01).collect();
        let b: Vec<f32> = (0..DIM).map(|i| 1.0 - i as f32 * 0.001).collect();
        let naive: f32 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
        assert!((dot(&a, &b) - naive).abs() < 1e-2);
    }
}
