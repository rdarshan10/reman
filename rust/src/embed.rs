//! Warm embedding model: bge-small-en-v1.5 (fp32 ONNX). Verified to reproduce the vectors the
//! Python fastembed stored (cosine 1.0000), and ~3x faster per query than the "-onnx-q" build on
//! this class of CPU. Plus a small query cache: typing, backspacing and re-typing in the finder
//! re-asks for the same strings constantly.
use crate::config::{self, DIM};
use anyhow::{Result, anyhow};
use fastembed::{EmbeddingModel, TextEmbedding, TextInitOptions};
use parking_lot::Mutex;
use std::collections::{HashMap, VecDeque};

pub struct Embedder {
    model: Mutex<TextEmbedding>,
    cache: Mutex<QueryCache>,
}

struct QueryCache {
    map: HashMap<String, std::sync::Arc<Vec<f32>>>,
    order: VecDeque<String>,
    cap: usize,
}

impl QueryCache {
    fn get(&self, k: &str) -> Option<std::sync::Arc<Vec<f32>>> {
        self.map.get(k).cloned()
    }
    fn put(&mut self, k: String, v: std::sync::Arc<Vec<f32>>) {
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

impl Embedder {
    pub fn load() -> Result<Self> {
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8);
        let opts = TextInitOptions::new(EmbeddingModel::BGESmallENV15)
            .with_cache_dir(config::models_dir())
            .with_intra_threads(threads)
            .with_show_download_progress(false);
        let model = TextEmbedding::try_new(opts).map_err(|e| anyhow!("loading embedding model: {e}"))?;
        Ok(Self {
            model: Mutex::new(model),
            cache: Mutex::new(QueryCache { map: HashMap::new(), order: VecDeque::new(), cap: 512 }),
        })
    }

    /// Normalised vectors, batched.
    pub fn embed_many(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        // length-sorted batches: one 3k-char pasted script no longer pads 63 short commands to 512 tokens
        let mut order: Vec<usize> = (0..texts.len()).collect();
        order.sort_by_key(|&i| texts[i].len());
        let sorted: Vec<&str> = order.iter().map(|&i| texts[i]).collect();
        let vecs = self.model.lock().embed(&sorted, Some(16)).map_err(|e| anyhow!("embed: {e}"))?;
        let mut out = vec![Vec::new(); texts.len()];
        for (mut v, &i) in vecs.into_iter().zip(&order) {
            debug_assert_eq!(v.len(), DIM);
            normalize(&mut v);
            out[i] = v;
        }
        Ok(out)
    }

    pub fn embed_query(&self, q: &str) -> Result<std::sync::Arc<Vec<f32>>> {
        if let Some(v) = self.cache.lock().get(q) {
            return Ok(v);
        }
        let v = std::sync::Arc::new(self.embed_many(&[q])?.remove(0));
        self.cache.lock().put(q.to_string(), v.clone());
        Ok(v)
    }
}
