//! Next-command prediction: "after X you usually run Y (here)". Bigram + trigram successor counts
//! mined from the execution log on demand (sub-millisecond at 10k runs, so no incremental state
//! can drift). A pair only counts if both runs were close in time AND share a folder or session,
//! so two shells interleaving in different projects don't pollute each other.
use crate::store::{Scope, Store};
use std::collections::HashMap;

const GAP: i64 = 300;
const CONTEXT_WINDOW: i64 = 2 * 3600;

pub struct Prediction {
    pub idx: u32,
    pub score: f32,
    pub count: u32,
    pub reason: String,
}

/// Most recent command (and the one before it) in this session/folder, if recent enough.
pub fn context(store: &Store, cwd: Option<u32>, session: Option<u32>) -> (Option<u32>, Option<u32>) {
    let now = crate::config::now();
    let mut it = store.execs.iter().rev().filter(|x| {
        now - x.ts <= CONTEXT_WINDOW
            && store.entries[x.entry as usize].alive
            && !store.entries[x.entry as usize].comment
            && match (session, cwd) {
                (Some(s), _) if !store.session_name(s).is_empty() => x.session == s,
                (_, Some(c)) => x.cwd == Some(c),
                _ => true,
            }
    });
    let last = it.next().map(|x| x.entry);
    let prev = it.find(|x| Some(x.entry) != last).map(|x| x.entry);
    (last, prev)
}

pub fn predict(store: &Store, cwd: Option<u32>, last: Option<u32>, prev: Option<u32>, k: usize) -> Vec<Prediction> {
    let mut score: HashMap<u32, (f32, u32)> = HashMap::new();
    if let Some(last) = last {
        let ex = &store.execs;
        for i in 1..ex.len() {
            let (a, b) = (&ex[i - 1], &ex[i]);
            if a.entry != last || b.entry == last || b.ts - a.ts > GAP {
                continue;
            }
            let linked = (a.cwd.is_some() && a.cwd == b.cwd) || (a.session == b.session && !store.session_name(a.session).is_empty());
            if !linked {
                continue;
            }
            let mut w = 1.0;
            if cwd.is_some() && b.cwd == cwd {
                w += 1.0; // same-folder evidence outweighs global
            }
            if i >= 2 && prev.is_some_and(|p| ex[i - 2].entry == p && ex[i - 1].ts - ex[i - 2].ts <= GAP) {
                w += 2.0; // trigram agrees
            }
            let s = score.entry(b.entry).or_default();
            s.0 += w;
            s.1 += 1;
        }
    }
    let mut out: Vec<Prediction> = score
        .into_iter()
        .filter(|(idx, _)| {
            let e = &store.entries[*idx as usize];
            e.recallable(false) && store.agg(e, Scope::All).status() != "fail"
        })
        .map(|(idx, (s, n))| Prediction {
            idx,
            score: s,
            count: n,
            reason: format!("after `{}` ({n}x)", truncate(&store.entries[last.unwrap() as usize].text, 40)),
        })
        .collect();
    out.sort_by(|a, b| b.score.total_cmp(&a.score).then(b.count.cmp(&a.count)));
    out.truncate(k);
    // top up with what's run most in this folder
    if out.len() < k {
        if let Some(c) = cwd {
            let mut freq: Vec<(u32, u32, i64)> = store
                .entries
                .iter()
                .enumerate()
                .filter(|(i, e)| e.recallable(false) && Some(*i as u32) != last && !out.iter().any(|p| p.idx == *i as u32))
                .filter_map(|(i, e)| {
                    let a = store.agg(e, Scope::Folder(c));
                    (a.runs >= 2 && a.status() != "fail").then_some((i as u32, a.runs, a.last_used))
                })
                .collect();
            freq.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.cmp(&a.2)));
            for (idx, runs, _) in freq.into_iter().take(k - out.len()) {
                out.push(Prediction { idx, score: 0.0, count: runs, reason: format!("often run here ({runs}x)") });
            }
        }
    }
    out
}

pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n - 1).collect::<String>()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{self, Run};

    #[test]
    fn learns_successor() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db::migrate(&db).unwrap();
        let mut s = Store::default();
        let now = crate::config::now();
        let mut t = now - 1000;
        for _ in 0..3 {
            for c in ["git add .", "git commit -m x", "git push"] {
                t += 10;
                let r = Run { cmd: c.into(), exit: Some(0), cwd: Some("C:/r".into()), session: "s1".into(), actor: "human".into(), ts: t, duration_ms: None, err: None };
                let rec = db::record_run(&db, &r).unwrap();
                s.apply_run(rec.command_id, rec.new_row, &r, None);
            }
        }
        let cwd = s.cwd_index("C:/r");
        let (last, prev) = context(&s, cwd, None);
        assert_eq!(s.entries[last.unwrap() as usize].text, "git push");
        let add = s.entry("git add .").unwrap().0;
        let p = predict(&s, cwd, Some(add), None, 3);
        assert_eq!(s.entries[p[0].idx as usize].text, "git commit -m x");
        let _ = prev;
    }
}
