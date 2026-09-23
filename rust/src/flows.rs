//! Workflow memory: recurring command SEQUENCES (runs executed back-to-back, gap < 5 min) that
//! recur >= min_count times. Same rules as the Python miner, over the in-memory execution log.
use crate::store::Store;
use std::collections::HashMap;

pub struct Flow {
    pub seq: Vec<u32>,
    pub count: u32,
}

fn is_periodic(g: &[u32]) -> bool {
    let l = g.len();
    (1..=l / 2).any(|p| l % p == 0 && (0..l).all(|i| g[i] == g[i % p]))
}

fn harvest(buf: &[u32], max_len: usize, grams: &mut HashMap<Vec<u32>, u32>) {
    for n in 2..=max_len {
        if buf.len() < n {
            break;
        }
        for w in buf.windows(n) {
            let distinct: std::collections::HashSet<_> = w.iter().collect();
            if distinct.len() < 2 {
                continue; // pure repeats (ls; ls; ls)
            }
            if w.windows(2).any(|p| p[0] == p[1]) {
                continue; // adjacent dup
            }
            if n >= 3 && distinct.len() == 2 {
                continue; // A-B-A toggle, not a workflow
            }
            if is_periodic(w) {
                continue; // A-B-C-A-B-C cycles
            }
            *grams.entry(w.to_vec()).or_default() += 1;
        }
    }
}

fn is_subseq(short: &[u32], long: &[u32]) -> bool {
    short.len() < long.len() && long.windows(short.len()).any(|w| w == short)
}

/// Runs of commands (entry indices) split on time gaps; with `cwd`, also split on any command
/// outside that folder.
pub fn runs(store: &Store, cwd: Option<u32>, gap: i64) -> Vec<Vec<u32>> {
    let mut out = Vec::new();
    let mut buf: Vec<u32> = Vec::new();
    let mut last: Option<i64> = None;
    for x in &store.execs {
        let e = &store.entries[x.entry as usize];
        if !e.alive || e.comment {
            continue;
        }
        if let Some(c) = cwd {
            if x.cwd != Some(c) {
                out.push(std::mem::take(&mut buf));
                last = Some(x.ts);
                continue;
            }
        }
        if last.is_some_and(|l| x.ts - l > gap) {
            out.push(std::mem::take(&mut buf));
        }
        buf.push(x.entry);
        last = Some(x.ts);
    }
    out.push(buf);
    out.retain(|b| !b.is_empty());
    out
}

pub fn detect(store: &Store, cwd: Option<u32>, min_count: u32, max_len: usize, gap: i64, limit: usize) -> Vec<Flow> {
    let mut grams: HashMap<Vec<u32>, u32> = HashMap::new();
    for r in runs(store, cwd, gap) {
        harvest(&r, max_len, &mut grams);
    }
    let mut flows: Vec<Flow> = grams.into_iter().filter(|(_, n)| *n >= min_count).map(|(seq, count)| Flow { seq, count }).collect();
    // deterministic order for equal keys
    flows.sort_by(|a, b| (b.seq.len(), b.count).cmp(&(a.seq.len(), a.count)).then_with(|| a.seq.cmp(&b.seq)));
    let mut kept: Vec<Flow> = Vec::new();
    for f in flows {
        if kept.iter().any(|k| is_subseq(&f.seq, &k.seq)) {
            continue;
        }
        kept.push(f);
    }
    kept.sort_by(|a, b| (b.count, b.seq.len()).cmp(&(a.count, a.seq.len())).then_with(|| a.seq.cmp(&b.seq)));
    kept.truncate(limit);
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filters() {
        assert!(is_periodic(&[1, 2, 1, 2]));
        assert!(is_periodic(&[1, 2, 3, 1, 2, 3]));
        assert!(!is_periodic(&[1, 2, 3]));
        let mut g = HashMap::new();
        harvest(&[1, 1, 1], 4, &mut g);
        assert!(g.is_empty());
        harvest(&[1, 2, 1], 4, &mut g); // toggle over 3 steps is dropped, pairs are kept
        assert!(!g.contains_key(&vec![1, 2, 1]));
        assert_eq!(g.get(&vec![1, 2]), Some(&1));
        assert!(is_subseq(&[2, 3], &[1, 2, 3]));
        assert!(!is_subseq(&[1, 3], &[1, 2, 3]));
    }
}
