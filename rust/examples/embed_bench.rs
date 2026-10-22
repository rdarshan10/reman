//! How well an embedding model matches a description to the raw command, on tools it never saw:
//! tools/build_cmd_pairs.py's test.jsonl, every description against every command.
//!
//!   cargo run --release --example embed_bench -- TEST.jsonl [ONNX_DIR]
//!
//! Without ONNX_DIR: the model reman ships (bge-small-en-v1.5). With it: model.onnx and the
//! tokenizer files in that folder (train_embedder.py's export). Prints top-1 / top-5 / MRR, with
//! the description as it is and in reman's search-query form, and how often the top command is
//! at least the right tool.
use fastembed::{EmbeddingModel, InitOptionsUserDefined, Pooling, TextEmbedding, TextInitOptions, TokenizerFiles, UserDefinedEmbeddingModel};

#[derive(serde::Deserialize)]
struct Row {
    tool: String,
    desc: String,
    cmd: String,
}

fn load(dir: Option<&str>) -> anyhow::Result<TextEmbedding> {
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8);
    Ok(match dir {
        None => TextEmbedding::try_new(
            TextInitOptions::new(EmbeddingModel::BGESmallENV15)
                .with_cache_dir(dirs::home_dir().unwrap().join(".reman/models"))
                .with_intra_threads(threads)
                .with_show_download_progress(false),
        )?,
        Some(d) => {
            let p = std::path::Path::new(d);
            let read = |f: &str| std::fs::read(p.join(f)).map_err(|e| anyhow::anyhow!("{f}: {e}"));
            let files = TokenizerFiles {
                tokenizer_file: read("tokenizer.json")?,
                config_file: read("config.json")?,
                special_tokens_map_file: read("special_tokens_map.json")?,
                tokenizer_config_file: read("tokenizer_config.json")?,
            };
            let model = UserDefinedEmbeddingModel::new(read("model.onnx")?, files).with_pooling(Pooling::Cls);
            TextEmbedding::try_new_from_user_defined(model, InitOptionsUserDefined::new().with_intra_threads(threads))?
        }
    })
}

fn embed(m: &mut TextEmbedding, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
    let mut out = m.embed(texts, Some(64))?;
    for v in &mut out {
        let n = v.iter().map(|x| x * x).sum::<f32>().sqrt() + 1e-9;
        v.iter_mut().for_each(|x| *x /= n);
    }
    Ok(out)
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let rows: Vec<Row> = std::fs::read_to_string(&args[1])?.lines().map(serde_json::from_str).collect::<Result<_, _>>()?;
    let mut m = load(args.get(2).map(String::as_str))?;
    let t = std::time::Instant::now();
    let cmds: Vec<String> = rows.iter().map(|r| r.cmd.clone()).collect();
    let cv = embed(&mut m, &cmds)?;
    println!("{} commands, {} tools; embedded in {:?}", rows.len(), rows.iter().map(|r| &r.tool).collect::<std::collections::HashSet<_>>().len(), t.elapsed());
    for (name, form) in [("as is", ""), ("search form", "Represent this sentence for searching relevant passages: ")] {
        let qs: Vec<String> = rows.iter().map(|r| format!("{form}{}", r.desc)).collect();
        let qv = embed(&mut m, &qs)?;
        let (mut top1, mut top5, mut mrr, mut tool1) = (0usize, 0usize, 0f64, 0usize);
        for (i, q) in qv.iter().enumerate() {
            let sims: Vec<f32> = cv.iter().map(|c| c.iter().zip(q).map(|(a, b)| a * b).sum()).collect();
            let right = |j: usize| rows[j].desc == rows[i].desc && rows[j].tool == rows[i].tool;
            // the rank of the best right command: how many commands score above it
            let best = (0..rows.len()).filter(|&j| right(j)).map(|j| sims[j]).fold(f32::MIN, f32::max);
            let rank = sims.iter().filter(|&&s| s > best).count();
            top1 += (rank == 0) as usize;
            top5 += (rank < 5) as usize;
            mrr += 1.0 / (rank as f64 + 1.0);
            let top = (0..rows.len()).max_by(|&a, &b| sims[a].total_cmp(&sims[b])).unwrap();
            tool1 += (rows[top].tool == rows[i].tool) as usize;
        }
        let n = rows.len() as f64;
        println!(
            "{name:<12} top-1 {:.1}%  top-5 {:.1}%  MRR {:.3}  right tool on top {:.1}%",
            100.0 * top1 as f64 / n,
            100.0 * top5 as f64 / n,
            mrr / n,
            100.0 * tool1 as f64 / n
        );
    }
    Ok(())
}
