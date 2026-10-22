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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};

pub struct Embedder {
    /// None until something needs it, and again once it has gone unused for a while
    model: Arc<Mutex<Option<TextEmbedding>>>,
    /// which model: its folder, or None for the base model (model.rs)
    folder: Arc<Mutex<Option<PathBuf>>>,
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

/// A request as a search query: bge-v1.5's instruction for retrieval
fn search_form(q: &str) -> String {
    format!("Represent this sentence for searching relevant passages: {q}")
}

/// A model in a folder of its own (model.onnx + tokenizer files, as tools/train_embedder.py
/// exports them): reman's own (model.rs), or REMAN_MODEL_DIR's.
fn load_from(dir: &Path, threads: usize) -> Result<TextEmbedding> {
    use fastembed::{InitOptionsUserDefined, Pooling, TokenizerFiles, UserDefinedEmbeddingModel};
    let read = |f: &str| std::fs::read(dir.join(f)).map_err(|e| anyhow!("{}: {e}", dir.join(f).display()));
    let files = TokenizerFiles {
        tokenizer_file: read("tokenizer.json")?,
        config_file: read("config.json")?,
        special_tokens_map_file: read("special_tokens_map.json")?,
        tokenizer_config_file: read("tokenizer_config.json")?,
    };
    let model = UserDefinedEmbeddingModel::new(read("model.onnx")?, files).with_pooling(Pooling::Cls);
    TextEmbedding::try_new_from_user_defined(model, InitOptionsUserDefined::new().with_intra_threads(threads)).map_err(|e| anyhow!("loading embedding model: {e}"))
}

/// How far the current model's similarities sit from the scale search.rs's close rule was
/// calibrated on (model::close_shift; f32 bits). Follows the model the Embedder uses.
static CLOSE_SHIFT: AtomicU32 = AtomicU32::new(0);

pub fn close_shift() -> f32 {
    f32::from_bits(CLOSE_SHIFT.load(Ordering::Relaxed))
}

/// REMAN_MODEL_DIR: a model to try (train_embedder.py's export). With it the daemon never
/// changes the model on its own; `reindex` re-embeds a history with it.
pub fn override_dir() -> Option<PathBuf> {
    std::env::var_os("REMAN_MODEL_DIR").filter(|d| !d.is_empty()).map(PathBuf::from)
}

/// Load a model: from its folder, or (None) fastembed's base model.
pub fn load(folder: Option<&Path>) -> Result<TextEmbedding> {
    let t = std::time::Instant::now();
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8);
    let mut model = match folder {
        Some(dir) => load_from(dir, threads)?,
        None => {
            let opts = TextInitOptions::new(EmbeddingModel::BGESmallENV15)
                .with_cache_dir(config::models_dir())
                .with_intra_threads(threads)
                .with_show_download_progress(false);
            TextEmbedding::try_new(opts).map_err(|e| anyhow!("loading embedding model: {e}"))?
        }
    };
    // the first run sets up the session's buffers: done here, not on someone's query
    model.embed(&["warmup query to load the model once"], None).map_err(|e| anyhow!("embed: {e}"))?;
    let name = folder.and_then(|d| d.file_name()).map_or(crate::model::BASE.into(), |n| n.to_string_lossy().into_owned());
    crate::daemon::log(&format!("embedding model {name} loaded in {:?}", t.elapsed()));
    Ok(model)
}

/// Normalised vectors of `texts` from `model`, in batches of about the same length: one 3k-char
/// pasted script no longer pads 63 short commands to 512 tokens.
pub fn embed_with(model: &mut TextEmbedding, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
    let mut order: Vec<usize> = (0..texts.len()).collect();
    order.sort_by_key(|&i| texts[i].len());
    let sorted: Vec<&str> = order.iter().map(|&i| texts[i]).collect();
    let mut vecs = Vec::with_capacity(sorted.len());
    for b in batches(&sorted) {
        let n = b.len();
        vecs.extend(model.embed(&sorted[b], Some(n)).map_err(|e| anyhow!("embed: {e}"))?);
    }
    let mut out = vec![Vec::new(); texts.len()];
    for (mut v, &i) in vecs.into_iter().zip(&order) {
        debug_assert_eq!(v.len(), DIM);
        normalize(&mut v);
        out[i] = v;
    }
    Ok(out)
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
    /// The model in `folder` (None: the base model), loaded when first needed.
    pub fn new(folder: Option<PathBuf>) -> Self {
        CLOSE_SHIFT.store(crate::model::close_shift(folder.as_deref()).to_bits(), Ordering::Relaxed);
        Self {
            model: Arc::new(Mutex::new(None)),
            folder: Arc::new(Mutex::new(folder)),
            loading: Arc::new(AtomicBool::new(false)),
            used: AtomicI64::new(config::now()),
            cache: Mutex::new(QueryCache { map: HashMap::new(), order: VecDeque::new(), cap: 512 }),
        }
    }

    /// Use another model from now on (once the history's vectors are from it: daemon.rs,
    /// upgrade_model). Waits for an embedding under way; cached query vectors are the old model's.
    pub fn switch(&self, folder: Option<PathBuf>) {
        let mut m = self.model.lock();
        let had = m.take().is_some();
        CLOSE_SHIFT.store(crate::model::close_shift(folder.as_deref()).to_bits(), Ordering::Relaxed);
        *self.folder.lock() = folder;
        let mut c = self.cache.lock();
        c.map.clear();
        c.order.clear();
        drop((m, c));
        if had {
            give_back();
        }
    }

    fn load_current(&self) -> Result<TextEmbedding> {
        let folder = self.folder.lock().clone();
        load(folder.as_deref())
    }

    /// Someone is using reman: load the model in the background if it isn't, so whatever comes
    /// next doesn't wait for it.
    pub fn prewarm(&self) {
        self.used.store(config::now(), Ordering::Relaxed);
        // busy = in use or loading right now
        if self.model.try_lock().is_none_or(|m| m.is_some()) || self.loading.swap(true, Ordering::AcqRel) {
            return;
        }
        let (model, folder, loading) = (self.model.clone(), self.folder.clone(), self.loading.clone());
        std::thread::spawn(move || {
            {
                let mut m = model.lock();
                if m.is_none() {
                    let f = folder.lock().clone();
                    match load(f.as_deref()) {
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
        let mut m = self.model.lock();
        if m.is_none() {
            *m = Some(self.load_current()?);
        }
        embed_with(m.as_mut().unwrap(), texts)
    }

    /// A request in words ("start the dev server") embedded as it is, and as a search query:
    /// bge-v1.5 was trained to take a search query with an instruction in front (the commands and
    /// descriptions it is matched against are embedded as they are), which orders commands
    /// better (search::search_ranked). One run of the model when neither is cached: a batch of
    /// two costs little more than one.
    pub fn embed_both(&self, q: &str) -> Result<(Arc<Vec<f32>>, Arc<Vec<f32>>)> {
        let s = search_form(q);
        let cached = {
            let c = self.cache.lock();
            (c.get(q), c.get(&s))
        };
        if let (Some(a), Some(b)) = cached {
            self.used.store(config::now(), Ordering::Relaxed);
            return Ok((a, b));
        }
        let mut v = self.embed_many(&[q, &s])?.into_iter().map(Arc::new);
        let (a, b) = (v.next().unwrap(), v.next().unwrap());
        let mut c = self.cache.lock();
        c.put(q.to_string(), a.clone());
        c.put(s, b.clone());
        Ok((a, b))
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
