//! Did-you-mean: for a failed command, the user's own real commands that resemble it.
//! Dual signal (typo closeness + intent similarity), exactly the Python weighting
//! (0.55 typo + 0.45 semantic), but OSA is pruned with a length bound so it stays in the
//! low milliseconds instead of Python's ~1s full scan.
use crate::search::dot;
use crate::store::{Scope, Store};

/// Optimal-string-alignment distance (Damerau-Levenshtein where an adjacent transposition = 1).
pub fn osa(a: &[char], b: &[char]) -> usize {
    let (la, lb) = (a.len(), b.len());
    if la == 0 {
        return lb;
    }
    if lb == 0 {
        return la;
    }
    let w = lb + 1;
    let mut d = vec![0usize; (la + 1) * w];
    for i in 0..=la {
        d[i * w] = i;
    }
    for j in 0..=lb {
        d[j] = j;
    }
    for i in 1..=la {
        for j in 1..=lb {
            let cost = (a[i - 1] != b[j - 1]) as usize;
            let mut v = (d[(i - 1) * w + j] + 1).min(d[i * w + j - 1] + 1).min(d[(i - 1) * w + j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(d[(i - 2) * w + j - 2] + 1);
            }
            d[i * w + j] = v;
        }
    }
    d[la * w + lb]
}

pub fn norm_edit(a: &str, b: &str) -> f32 {
    let (ac, bc): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    osa(&ac, &bc) as f32 / ac.len().max(bc.len()).max(1) as f32
}

pub struct Suggestion {
    pub idx: u32,
    pub score: f32,
    pub typo: f32,
    pub sem: f32,
}

/// `qv` = embedding of the failed command (None -> typo only). worked_only = success pool.
pub fn did_you_mean(store: &Store, failed: &str, qv: Option<&[f32]>, k: usize, worked_only: bool, scope: Scope) -> Vec<Suggestion> {
    const MAXLEN: usize = 300;
    let k = k.max(1);
    let fc: Vec<char> = failed.chars().take(MAXLEN).collect();
    struct C {
        idx: u32,
        sem: f32,
        ub: f32,
    }
    let mut cands: Vec<C> = Vec::new();
    for (i, e) in store.entries.iter().enumerate() {
        if !e.recallable(false) || e.text == failed || !store.in_scope(e, scope) {
            continue;
        }
        // "worked" pool: anything not known to only fail - old history never recorded exit codes,
        // and `docker ps` run 8x with no outcome is still the right answer to `dcoker ps`
        if worked_only && {
            let a = store.agg(e, scope);
            a.ok == 0 && a.fail > 0
        } {
            continue;
        }
        let sem = match qv {
            Some(v) if e.has_vec => dot(store.vec(i as u32), v),
            _ => 0.0,
        };
        // typo upper bound from the length difference alone: dist >= |la - lb|
        let lb = e.text.chars().count().min(MAXLEN);
        let typo_ub = 1.0 - (fc.len().abs_diff(lb) as f32 / fc.len().max(lb).max(1) as f32);
        cands.push(C { idx: i as u32, sem, ub: 0.55 * typo_ub + 0.45 * sem });
    }
    cands.sort_by(|a, b| b.ub.total_cmp(&a.ub));
    // best per group_key, pruned: once the upper bound can't beat the k-th best group, stop.
    let mut best: Vec<Suggestion> = Vec::new();
    let mut by_group: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for c in cands {
        if best.len() >= k * 4 {
            let mut scores: Vec<f32> = best.iter().map(|s| s.score).collect();
            scores.sort_by(|a, b| b.total_cmp(a));
            if c.ub < scores[k - 1] {
                break;
            }
        }
        let e = &store.entries[c.idx as usize];
        let ec: Vec<char> = e.text.chars().take(MAXLEN).collect();
        let typo = 1.0 - osa(&fc, &ec) as f32 / fc.len().max(ec.len()).max(1) as f32;
        let score = 0.55 * typo + 0.45 * c.sem;
        let s = Suggestion { idx: c.idx, score, typo, sem: c.sem };
        match by_group.get(e.gkey.as_str()) {
            Some(&pos) if best[pos].score >= score => {}
            Some(&pos) => best[pos] = s,
            None => {
                by_group.insert(e.gkey.as_str(), best.len());
                best.push(s);
            }
        }
    }
    best.sort_by(|a, b| b.score.total_cmp(&a.score));
    best.truncate(k);
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transposition_costs_one() {
        let c = |s: &str| s.chars().collect::<Vec<_>>();
        assert_eq!(osa(&c("gti"), &c("git")), 1);
        assert_eq!(osa(&c("pip install nummpy"), &c("pip install numpy")), 1);
        assert_eq!(osa(&c(""), &c("abc")), 3);
        assert!(norm_edit("pip install nummpy", "pip install numpy") <= 0.15);
    }
}
