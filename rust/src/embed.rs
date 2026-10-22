//! Warm embedding model: bge-small-en-v1.5 (fp32 ONNX). Verified to reproduce the vectors the
//! Python fastembed stored (cosine 1.0000), and ~3x faster per query than the "-onnx-q" build on
//! this class of CPU. Plus a small query cache: typing, backspacing and re-typing in the finder
//! re-asks for the same strings constantly.
//!
//! The model is most of the daemon's memory (~165 MB of its weights), so it is loaded when first
//! needed, not when the daemon starts (often at boot, by an editor or a shell nobody types in),
//! and let go after a while with nobody using reman. Any request warms it in the background
//! (`prewarm`), so the search or failed command that follows finds it loaded.
use crate::config::{self, DIM};
use anyhow::{Result, anyhow};
use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

pub struct Embedder {
    /// None until something needs it, and again once it has gone unused for a while
    model: Arc<Mutex<Option<TextEmbedding>>>,
    /// a background load is under way
    loading: Arc<AtomicBool>,
    /// when reman was last used (unix seconds)
    used: AtomicI64,
    cache: Mutex<QueryCache>,
}

struct QueryCache {
    map: HashMap<String, Arc<Vec<f32>>>,
    order: VecDeque<String>,
    cap: usize,
}

impl QueryCache {
    fn get(&self, k: &str) -> Option<Arc<Vec<f32>>> {
        self.map.get(k).cloned()
    }
    fn put(&mut self, k: String, v: Arc<Vec<f32>>) {
        if self.map.len() >= self.cap {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
        self.order.push_back(k.clone());
        self.map.insert(k, v);
    }
}

pub fn normalize(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt() + 1e-9;
    v.iter_mut().for_each(|x| *x /= n);
}

fn load_model() -> Result<TextEmbedding> {
    let t = std::time::Instant::now();
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8);
    let opts = TextInitOptions::new(EmbeddingModel::BGESmallENV15)
        .with_cache_dir(config::models_dir())
        .with_intra_threads(threads)
        .with_show_download_progress(false);
    let mut model = TextEmbedding::try_new(opts).map_err(|e| anyhow!("loading embedding model: {e}"))?;
    // the first run sets up the session's buffers: done here, not on someone's query
    model.embed(&["warmup query to load the model once"], None).map_err(|e| anyhow!("embed: {e}"))?;
    crate::daemon::log(&format!("embedding model loaded in {:?}", t.elapsed()));
    Ok(model)
}

/// What the model freed goes back to the system: the heap keeps freed blocks committed for reuse
/// otherwise, which after the model is a few dozen MB nobody will reuse soon.
fn give_back() {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetProcessHeap() -> isize;
            fn HeapCompact(heap: isize, flags: u32) -> usize;
        }
        // SAFETY: compacts the process heap (no flags); safe while other threads allocate
        unsafe {
            HeapCompact(GetProcessHeap(), 0);
        }
    }
}

/// Batches of texts of about the same length (they come sorted), at most this many characters
/// of padded text each: the longest text times the batch's size. Bounds what one run of the
/// model allocates (16 pasted scripts padded to 512 tokens took ~1 GB, and kept it) and is
/// faster too: less padding, and buffers that stay in cache.
const BATCH_CHARS: usize = 1024;
const BATCH_MAX: usize = 32;

fn batches(sorted: &[&str]) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < sorted.len() {
        let mut j = i + 1;
        while j < sorted.len() && j - i < BATCH_MAX && (j + 1 - i) * sorted[j].len().max(32) <= BATCH_CHARS {
            j += 1;
        }
        out.push(i..j);
        i = j;
    }
    out
}

impl Embedder {
    /// Nothing is loaded yet.
    pub fn new() -> Self {
        Self {
            model: Arc::new(Mutex::new(None)),
            loading: Arc::new(AtomicBool::new(false)),
            used: AtomicI64::new(config::now()),
            cache: Mutex::new(QueryCache { map: HashMap::new(), order: VecDeque::new(), cap: 512 }),
        }
    }

    /// Someone is using reman: load the model in the background if it isn't, so whatever comes
    /// next doesn't wait for it.
    pub fn prewarm(&self) {
        self.used.store(config::now(), Ordering::Relaxed);
        // busy = in use or loading right now
        if self.model.try_lock().is_none_or(|m| m.is_some()) || self.loading.swap(true, Ordering::AcqRel) {
            return;
        }
        let (model, loading) = (self.model.clone(), self.loading.clone());
        std::thread::spawn(move || {
            {
                let mut m = model.lock();
                if m.is_none() {
                    match load_model() {
                        Ok(x) => *m = Some(x),
                        Err(e) => crate::daemon::log(&format!("{e:#}")),
                    }
                }
            }
            loading.store(false, Ordering::Release);
        });
    }

    /// Let the model go when reman hasn't been used for `idle_s` seconds. True when it did.
    pub fn release_if_idle(&self, idle_s: i64) -> bool {
        if config::now() - self.used.load(Ordering::Relaxed) < idle_s {
            return false;
        }
        let Some(mut m) = self.model.try_lock() else { return false };
        let released = m.take().is_some();
        drop(m);
        if released {
            give_back();
            crate::daemon::log(&format!("embedding model released after {} min unused", idle_s / 60));
        }
        released
    }

    /// Normalised vectors, batched.
    pub fn embed_many(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        self.used.store(config::now(), Ordering::Relaxed);
        // length-sorted batches: one 3k-char pasted script no longer pads 63 short commands to 512 tokens
        let mut order: Vec<usize> = (0..texts.len()).collect();
        order.sort_by_key(|&i| texts[i].len());
        let sorted: Vec<&str> = order.iter().map(|&i| texts[i]).collect();
        let mut vecs = Vec::with_capacity(sorted.len());
        {
            let mut m = self.model.lock();
            if m.is_none() {
                *m = Some(load_model()?);
            }
            let model = m.as_mut().unwrap();
            for b in batches(&sorted) {
                let n = b.len();
                vecs.extend(model.embed(&sorted[b], Some(n)).map_err(|e| anyhow!("embed: {e}"))?);
            }
        }
        let mut out = vec![Vec::new(); texts.len()];
        for (mut v, &i) in vecs.into_iter().zip(&order) {
            debug_assert_eq!(v.len(), DIM);
            normalize(&mut v);
            out[i] = v;
        }
        Ok(out)
    }

    pub fn embed_query(&self, q: &str) -> Result<Arc<Vec<f32>>> {
        if let Some(v) = self.cache.lock().get(q) {
            self.used.store(config::now(), Ordering::Relaxed);
            return Ok(v);
        }
        let v = Arc::new(self.embed_many(&[q])?.remove(0));
        self.cache.lock().put(q.to_string(), v.clone());
        Ok(v)
    }
}

#[cfg(test)]
mod tests {
    use super::batches;

    #[test]
    fn batches_hold_many_short_texts_and_few_long_ones() {
        let short = vec!["git status"; 70];
        let b = batches(&short);
        assert_eq!(b.iter().map(|r| r.len()).collect::<Vec<_>>(), vec![32, 32, 6]);
        let long = "x".repeat(900);
        let mixed: Vec<&str> = vec!["ls", "ls", &long, &long];
        let b = batches(&mixed);
        // the long ones go alone; every text is in exactly one batch, in order
        assert_eq!(b, vec![0..2, 2..3, 3..4]);
        assert!(batches(&[]).is_empty());
    }
}
