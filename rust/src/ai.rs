//! An optional language model, spoken to over the OpenAI-compatible chat API, so a local model
//! works as well as a hosted one (Ollama, LM Studio, llama.cpp, vLLM, OpenAI, OpenRouter...).
//!
//! reman never lets a model invent a command. The model only ARRANGES and EXPLAINS commands that
//! really worked (the runbook: a summary, a getting-started order, a few words per command, a
//! section for commands the fixed rules can't place), and every command in its answer must be
//! one it was given, character for character, or it is dropped. With no model, or on any error,
//! everything works as before (the static runbook).
//!
//! Which model: `ai` in ~/.reman/config.json if set; otherwise a server on this machine (Ollama
//! :11434, LM Studio :1234, llama.cpp :8080). A remote endpoint is only ever used when set there,
//! and commands are redacted before they are sent anywhere.
use crate::config;
use crate::insight::SECTIONS;
use crate::redact;
use crate::settings;
use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::time::Duration;

const LOCAL: [&str; 3] = ["http://127.0.0.1:11434/v1", "http://127.0.0.1:1234/v1", "http://127.0.0.1:8080/v1"];

pub struct Model {
    pub endpoint: String,
    pub model: String,
    key: Option<String>,
    /// on this machine: nothing leaves it
    pub local: bool,
}

impl Model {
    pub fn label(&self) -> String {
        format!("{} at {}", self.model, self.endpoint)
    }
}

fn is_local(endpoint: &str) -> bool {
    let rest = endpoint.split("://").nth(1).unwrap_or(endpoint);
    let host = rest.split(['/', ':']).next().unwrap_or("");
    matches!(host, "127.0.0.1" | "localhost" | "[::1]" | "::1")
}

fn agent(timeout: Duration, local: bool) -> ureq::Agent {
    let mut b = ureq::Agent::config_builder().timeout_global(Some(timeout));
    if local {
        // a proxy from the environment must not swallow calls to this machine
        b = b.proxy(None);
    }
    b.build().into()
}

fn get_models(endpoint: &str, key: Option<&str>, timeout: Duration) -> Result<Vec<String>> {
    let mut req = agent(timeout, is_local(endpoint)).get(format!("{}/models", endpoint.trim_end_matches('/')));
    if let Some(k) = key {
        req = req.header("Authorization", format!("Bearer {k}"));
    }
    let body = req.call()?.body_mut().read_to_string()?;
    let v: Value = serde_json::from_str(&body)?;
    Ok(v["data"].as_array().into_iter().flatten().filter_map(|m| m["id"].as_str().map(String::from)).collect())
}

/// A chat model, not an embedding one.
fn pick(models: &[String]) -> Option<String> {
    models.iter().find(|m| !m.to_lowercase().contains("embed")).cloned()
}

/// The model to use, if any: the configured one, else one running on this machine.
pub fn find() -> Option<Model> {
    let cfg = settings::load().ai;
    if cfg.as_ref().is_some_and(|a| !a.enabled) {
        return None;
    }
    let want = cfg.as_ref().and_then(|a| a.model.clone());
    if let Some(ep) = cfg.as_ref().and_then(|a| a.endpoint.clone()) {
        let var = cfg.as_ref().and_then(|a| a.api_key_env.clone()).unwrap_or_else(|| "REMAN_AI_KEY".into());
        let key = std::env::var(&var).ok().filter(|k| !k.is_empty());
        let model = match want {
            Some(m) => m,
            None => pick(&get_models(&ep, key.as_deref(), Duration::from_secs(5)).ok()?)?,
        };
        let local = is_local(&ep);
        return Some(Model { endpoint: ep.trim_end_matches('/').to_string(), model, key, local });
    }
    for ep in LOCAL {
        if let Ok(models) = get_models(ep, None, Duration::from_millis(400)) {
            let model = want.clone().filter(|w| models.contains(w)).or_else(|| pick(&models));
            if let Some(model) = model {
                return Some(Model { endpoint: ep.to_string(), model, key: None, local: true });
            }
        }
    }
    None
}

/// One chat completion; the reply's text.
pub fn chat(m: &Model, system: &str, user: &str, timeout: Duration) -> Result<String> {
    let body = json!({"model": m.model, "temperature": 0.2, "stream": false,
                      "messages": [{"role": "system", "content": system}, {"role": "user", "content": user}]});
    let mut req = agent(timeout, m.local).post(format!("{}/chat/completions", m.endpoint)).header("Content-Type", "application/json");
    if let Some(k) = &m.key {
        req = req.header("Authorization", format!("Bearer {k}"));
    }
    let text = req.send(body.to_string())?.body_mut().read_to_string()?;
    let v: Value = serde_json::from_str(&text).context("the model's reply is not JSON")?;
    v["choices"][0]["message"]["content"].as_str().map(String::from).ok_or_else(|| anyhow!("no message in the model's reply"))
}

/// The first JSON object in a reply (models like to wrap it in ```json fences or prose).
fn json_in(text: &str) -> Option<Value> {
    let (a, b) = (text.find('{')?, text.rfind('}')?);
    serde_json::from_str(text.get(a..=b)?).ok()
}

// ---------------------------------------------------------------------------------------------
// runbook
// ---------------------------------------------------------------------------------------------

/// What the model added: a summary, a getting-started order, notes per command, and sections
/// for commands no rule placed. Keyed by the ORIGINAL command text.
#[derive(Default)]
pub struct Extra {
    pub summary: String,
    pub getting_started: Vec<String>,
    pub notes: HashMap<String, String>,
    pub placed: Vec<(String, String)>,
}

fn commands_of(rb: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in rb["sections"].as_array().into_iter().flatten() {
        out.extend(s["commands"].as_array().into_iter().flatten().filter_map(|c| c["command"].as_str().map(String::from)));
    }
    out.extend(rb["unplaced"].as_array().into_iter().flatten().filter_map(|c| c["command"].as_str().map(String::from)));
    out
}

const SYSTEM: &str = "You write a short runbook for a software project from the developer's own shell history. \
Use ONLY commands from the lists you are given, copied exactly, character for character. Never invent, edit or combine commands. \
If you are unsure what a command does, leave it out. Answer with one JSON object and nothing else.";

/// Ask the model for the runbook's extras. `rb` is the static runbook with `unplaced`.
pub fn runbook(m: &Model, rb: &Value) -> Result<Extra> {
    // what may be sent: redacted, and never anything still secret-looking
    let mut to_orig: HashMap<String, String> = HashMap::new();
    for c in commands_of(rb) {
        if let Some(safe) = redact::safe_command(&c, true) {
            to_orig.entry(safe).or_insert(c);
        }
    }
    let list = |v: &Value| -> Vec<Value> {
        v.as_array()
            .into_iter()
            .flatten()
            .filter_map(|c| {
                let t = redact::safe_command(c["command"].as_str()?, true)?;
                Some(json!({"command": t, "runs": c["runs"], "worked": c["worked"]}))
            })
            .collect()
    };
    let sections: Vec<Value> = rb["sections"].as_array().into_iter().flatten().map(|s| json!({"title": s["title"], "commands": list(&s["commands"])})).collect();
    let flows: Vec<Value> = rb["flows"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| {
            let steps: Option<Vec<String>> = f["steps"].as_array()?.iter().map(|s| redact::safe_command(s.as_str()?, true)).collect();
            steps.map(|s| json!({"steps": s, "times": f["count"]}))
        })
        .collect();
    let project = rb["folder"].as_str().map(|f| f.replace('\\', "/")).and_then(|f| f.rsplit('/').find(|p| !p.is_empty()).map(String::from)).unwrap_or_default();
    let user = json!({
        "project": project,
        "sections": sections,
        "other_frequent_commands": list(&rb["unplaced"]),
        "usual_sequences": flows,
        "allowed_section_titles": SECTIONS,
        "reply_with": {
            "summary": "one or two sentences: what this project is and how it is run, judging by the commands",
            "getting_started": ["the commands a newcomer runs first, in order, max 6, exact commands from the lists"],
            "notes": [{"command": "an exact command from the lists", "note": "what it does in this project, max 12 words"}],
            "place": [{"command": "an exact command from other_frequent_commands that clearly belongs to a section", "section": "one of allowed_section_titles"}]
        }
    });
    let reply = chat(m, SYSTEM, &serde_json::to_string_pretty(&user)?, Duration::from_secs(180))?;
    let v = json_in(&reply).ok_or_else(|| anyhow!("the model did not answer with JSON"))?;
    Ok(validate(&v, &to_orig))
}

/// Keep only what refers to commands the model was given, exactly; cap the lengths.
fn validate(v: &Value, to_orig: &HashMap<String, String>) -> Extra {
    let exact = |s: &Value| s.as_str().map(str::trim).and_then(|t| to_orig.get(t)).cloned();
    let clip = |s: &str, n: usize| {
        let s = s.trim().replace('\n', " ");
        if s.chars().count() > n { format!("{}…", s.chars().take(n - 1).collect::<String>()) } else { s }
    };
    let mut x = Extra { summary: v["summary"].as_str().map(|s| clip(s, 300)).unwrap_or_default(), ..Default::default() };
    for c in v["getting_started"].as_array().into_iter().flatten() {
        if let Some(o) = exact(c) {
            if !x.getting_started.contains(&o) && x.getting_started.len() < 6 {
                x.getting_started.push(o);
            }
        }
    }
    for n in v["notes"].as_array().into_iter().flatten() {
        if let (Some(o), Some(note)) = (exact(&n["command"]), n["note"].as_str().filter(|s| !s.trim().is_empty())) {
            x.notes.insert(o, clip(note, 100));
        }
    }
    for p in v["place"].as_array().into_iter().flatten() {
        if let (Some(o), Some(sec)) = (exact(&p["command"]), p["section"].as_str().and_then(|s| SECTIONS.iter().find(|t| t.eq_ignore_ascii_case(s.trim())))) {
            x.placed.push((o, sec.to_string()));
        }
    }
    x
}

/// The static runbook with the model's extras: summary, getting started, a note per command,
/// and the commands it placed (only ones from `unplaced`, with their real record).
pub fn merge(rb: &Value, x: &Extra, model: &str) -> Value {
    let mut out = rb.clone();
    let unplaced: HashMap<String, Value> =
        rb["unplaced"].as_array().into_iter().flatten().filter_map(|c| Some((c["command"].as_str()?.to_string(), c.clone()))).collect();
    let mut sections: Vec<Value> = rb["sections"].as_array().cloned().unwrap_or_default();
    for (cmd, title) in &x.placed {
        let Some(rec) = unplaced.get(cmd) else { continue };
        let slot = match sections.iter().position(|s| s["title"] == json!(title)) {
            Some(i) => i,
            None => {
                sections.push(json!({"title": title, "commands": []}));
                sections.len() - 1
            }
        };
        if let Some(cmds) = sections[slot]["commands"].as_array_mut() {
            if !cmds.iter().any(|c| c["command"] == json!(cmd)) {
                cmds.push(rec.clone());
            }
        }
    }
    // the fixed section order
    sections.sort_by_key(|s| SECTIONS.iter().position(|t| s["title"] == json!(t)).unwrap_or(SECTIONS.len()));
    for s in sections.iter_mut() {
        for c in s["commands"].as_array_mut().into_iter().flatten() {
            if let Some(n) = c["command"].as_str().and_then(|t| x.notes.get(t)) {
                c["note"] = json!(n);
            }
        }
    }
    out["sections"] = json!(sections);
    out["summary"] = json!(x.summary);
    out["getting_started"] = json!(x.getting_started);
    out["model"] = json!(model);
    out["found"] = json!(true);
    if let Some(o) = out.as_object_mut() {
        o.remove("unplaced");
    }
    out
}

/// The runbook without the list only the model needs.
pub fn without_unplaced(mut rb: Value) -> Value {
    if let Some(o) = rb.as_object_mut() {
        o.remove("unplaced");
    }
    rb
}

/// For agents, who must never wait on a slow local model: write the fuller runbook in the
/// background, so their next call gets it. One at a time per project; after finding no model,
/// don't look again for 10 minutes.
pub fn generate_in_background(rb: Value) {
    use parking_lot::Mutex;
    use std::sync::OnceLock;
    static BUSY: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();
    static NO_MODEL_UNTIL: Mutex<i64> = Mutex::new(0);
    if config::now() < *NO_MODEL_UNTIL.lock() {
        return;
    }
    let folder = rb["folder"].as_str().unwrap_or("").to_string();
    if !BUSY.get_or_init(Default::default).lock().insert(folder.clone()) {
        return;
    }
    std::thread::spawn(move || {
        match find() {
            None => *NO_MODEL_UNTIL.lock() = config::now() + 600,
            Some(m) => {
                if let Ok(x) = runbook(&m, &rb) {
                    let _ = save(&rb, &x, &m.model);
                }
            }
        }
        BUSY.get_or_init(Default::default).lock().remove(&folder);
    });
}

// ---------------------------------------------------------------------------------------------
// cache: one file per project, valid while the runbook's commands are the same
// ---------------------------------------------------------------------------------------------

fn digest(s: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(s.as_bytes()).iter().take(12).map(|b| format!("{b:02x}")).collect()
}

/// What the extras depend on: the commands and sequences, not their counts or ages.
fn key(rb: &Value) -> String {
    let flows: Vec<String> = rb["flows"].as_array().into_iter().flatten().map(|f| f["steps"].to_string()).collect();
    digest(&format!("{:?}|{:?}", commands_of(rb), flows))
}

/// Next to the history (`~/.reman/runbooks/`), so a sandboxed REMAN_DB keeps its own.
fn cache_path(rb: &Value) -> std::path::PathBuf {
    let db = config::db_path();
    let dir = db.parent().map(|p| p.to_path_buf()).unwrap_or_else(config::home);
    dir.join("runbooks").join(format!("{}.json", digest(rb["folder"].as_str().unwrap_or(""))))
}

pub fn save(rb: &Value, x: &Extra, model: &str) -> Result<()> {
    let p = cache_path(rb);
    std::fs::create_dir_all(p.parent().unwrap())?;
    let placed: Vec<Value> = x.placed.iter().map(|(c, s)| json!({"command": c, "section": s})).collect();
    let v = json!({"key": key(rb), "model": model, "summary": x.summary, "getting_started": x.getting_started, "notes": x.notes, "placed": placed});
    std::fs::write(&p, serde_json::to_string_pretty(&v)?)?;
    Ok(())
}

/// The cached extras for this runbook, while its commands haven't changed: (extras, model).
pub fn cached(rb: &Value) -> Option<(Extra, String)> {
    let v: Value = serde_json::from_str(&std::fs::read_to_string(cache_path(rb)).ok()?).ok()?;
    if v["key"].as_str() != Some(key(rb).as_str()) {
        return None;
    }
    let x = Extra {
        summary: v["summary"].as_str().unwrap_or("").to_string(),
        getting_started: v["getting_started"].as_array().into_iter().flatten().filter_map(|s| s.as_str().map(String::from)).collect(),
        notes: v["notes"].as_object().into_iter().flatten().filter_map(|(k, n)| Some((k.clone(), n.as_str()?.to_string()))).collect(),
        placed: v["placed"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|p| Some((p["command"].as_str()?.to_string(), p["section"].as_str()?.to_string())))
            .collect(),
    };
    Some((x, v["model"].as_str().unwrap_or("").to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_hosts() {
        assert!(is_local("http://127.0.0.1:11434/v1"));
        assert!(is_local("http://localhost:1234/v1"));
        assert!(!is_local("https://api.openai.com/v1"));
    }

    #[test]
    fn json_in_fenced_replies() {
        assert_eq!(json_in("```json\n{\"a\": 1}\n```").unwrap()["a"], 1);
        assert!(json_in("no json here").is_none());
    }

    #[test]
    fn a_model_cannot_invent_commands() {
        let to_orig: HashMap<String, String> = [("npm test".to_string(), "npm test".to_string()), ("npm run dev".to_string(), "npm run dev".to_string())].into();
        let reply = json!({
            "summary": "A Node app.",
            "getting_started": ["npm ci", "npm run dev", "npm run dev"],
            "notes": [{"command": "npm test", "note": "runs the unit tests"}, {"command": "rm -rf /", "note": "cleans"}],
            "place": [{"command": "npm test", "section": "test"}, {"command": "make deploy", "section": "Deploy and release"}, {"command": "npm run dev", "section": "Nonsense"}]
        });
        let x = validate(&reply, &to_orig);
        assert_eq!(x.getting_started, vec!["npm run dev"]);
        assert_eq!(x.notes.len(), 1);
        assert_eq!(x.placed, vec![("npm test".to_string(), "Test".to_string())]);
    }
}
