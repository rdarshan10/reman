//! Which embedding model the history is embedded with, and getting reman's own.
//!
//! reman started on bge-small-en-v1.5 as published (fastembed downloads it). Its own model is the
//! same network fine-tuned on commands (tools/train_embedder.py): tldr's descriptions and casual
//! paraphrases of them, so it knows `git branch -D` deletes a branch and "spin up the containers"
//! means `docker compose up`. It is a zip on a GitHub pre-release; its SHA-256 is built into
//! reman, so no other file is ever used.
//!
//! Every stored vector must come from the model that embeds the queries: the database records
//! which one made them (meta `embed_model`), and the daemon re-embeds the history before it
//! switches (daemon.rs, `upgrade_model`).
use crate::{config, settings};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

/// The model bge-small-en-v1.5 is known by: fastembed's own copy, no folder of ours.
pub const BASE: &str = "bge-small-en-v1.5";

pub struct Spec {
    pub id: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
}

/// reman's model. Published as a GitHub pre-release (never "latest": update.rs reads the latest
/// release as reman's newest version).
pub const COMMANDS: Spec = Spec {
    id: "reman-commands-1",
    url: "https://github.com/rdarshan10/reman/releases/download/model-commands-1/reman-commands-1.zip",
    sha256: "df282e043935cece73ea83191433a15e393163722fa300127fb40326072cb4c7",
};

/// No real model is this big; a bigger download is not ours.
const MAX_ZIP: u64 = 400 << 20;
/// The files a model folder must hold (as train_embedder.py exports them).
const FILES: [&str; 6] = ["model.onnx", "tokenizer.json", "tokenizer_config.json", "special_tokens_map.json", "config.json", "reman.json"];

/// Where a model's files live once downloaded and checked.
pub fn dir(id: &str) -> PathBuf {
    config::models_dir().join(id)
}

/// Downloaded, checked and unpacked (the marker is written last).
pub fn ready(id: &str) -> bool {
    dir(id).join(".verified").exists()
}

/// The model reman should use: its own, unless config.json says `"embed_model": "base"`.
pub fn wanted() -> &'static str {
    match settings::load().embed_model.as_deref() {
        Some("base") => BASE,
        _ => COMMANDS.id,
    }
}

/// The folder to load a model from; None for the base model (fastembed's own).
pub fn folder(id: &str) -> Option<PathBuf> {
    (id != BASE).then(|| dir(id))
}

/// The test suites' copy of the model: a zip on this machine and its checksum, honoured only for a
/// sandbox (REMAN_HOME set), like update.rs's test server.
fn source() -> (String, String) {
    let sandbox = std::env::var_os("REMAN_HOME").is_some_and(|h| !h.is_empty());
    match (std::env::var("REMAN_MODEL_URL"), std::env::var("REMAN_MODEL_SHA256")) {
        (Ok(url), Ok(sum)) if sandbox && crate::update::on_this_machine(&url) => (url, sum.to_ascii_lowercase()),
        _ => (COMMANDS.url.to_string(), COMMANDS.sha256.to_string()),
    }
}

/// Download reman's model, check it against the checksum built into reman, and unpack it.
pub fn fetch() -> Result<()> {
    let id = COMMANDS.id;
    if ready(id) {
        return Ok(());
    }
    let (url, sum) = source();
    if sum.bytes().all(|b| b == b'0') {
        bail!("this build of reman names no model to download");
    }
    let root = config::models_dir();
    std::fs::create_dir_all(&root)?;
    let zip = root.join(format!("{id}.zip.part"));
    let tmp = root.join(format!("{id}.part"));
    let _ = std::fs::remove_dir_all(&tmp);
    crate::update::download(&url, &zip, MAX_ZIP)?;
    let got = crate::update::sha256_of(&zip)?;
    if got != sum {
        let _ = std::fs::remove_file(&zip);
        bail!("the downloaded model is not the one reman knows (sha256 {got}); not using it");
    }
    std::fs::create_dir_all(&tmp)?;
    crate::update::unpack(&zip, &tmp)?;
    let _ = std::fs::remove_file(&zip);
    for f in FILES {
        if !tmp.join(f).is_file() {
            let _ = std::fs::remove_dir_all(&tmp);
            bail!("the model download has no {f}");
        }
    }
    let _ = std::fs::remove_dir_all(dir(id));
    std::fs::rename(&tmp, dir(id)).context("putting the model in place")?;
    std::fs::write(dir(id).join(".verified"), &got)?;
    Ok(())
}

/// How far a model's similarities sit from the scale search.rs's close rule was calibrated on (the
/// base model's): from `reman.json` beside it, `{"close_shift": -0.24}`.
pub fn close_shift(folder: Option<&Path>) -> f32 {
    folder
        .and_then(|d| std::fs::read_to_string(d.join("reman.json")).ok())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("close_shift").and_then(serde_json::Value::as_f64))
        .unwrap_or(0.0) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_base_model_has_no_folder_and_no_shift() {
        assert!(folder(BASE).is_none());
        assert_eq!(close_shift(None), 0.0);
        assert!(folder(COMMANDS.id).is_some_and(|d| d.ends_with(COMMANDS.id)));
    }

    #[test]
    fn a_test_model_is_honoured_only_on_this_machine() {
        // without a sandbox, the built-in source, whatever the environment says
        if std::env::var_os("REMAN_HOME").is_none() {
            assert_eq!(source().0, COMMANDS.url);
        }
        assert!(COMMANDS.url.starts_with("https://github.com/rdarshan10/reman/releases/download/"));
    }
}
