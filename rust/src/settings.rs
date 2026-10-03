//! ~/.reman/config.json - the agent-facing policy shared by every connector: which folders agents
//! may see (the security boundary), strict secret mode, and the optional local HTTP endpoint.
use crate::config;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct Settings {
    /// folders whose commands agents may read (REMAN_MCP_ROOT overrides per agent)
    #[serde(default)]
    pub mcp_roots: Vec<String>,
    #[serde(default = "yes")]
    pub strict_secrets: bool,
    /// history imported before reman recorded folders has no folder, so no root can contain it.
    /// When on, agents also get the GENERIC ones (well-known tool, no paths/quotes/hosts/vars -
    /// see describe::is_generic), labelled as folder-unknown.
    #[serde(default)]
    pub share_old_history: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<Http>,
    /// the optional language model (see ai.rs); absent = use a local one if one is running
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai: Option<Ai>,
    /// a command holding a secret, when recorded: "mask" keeps it with the value hidden
    /// (`export API_TOKEN=***`, the default), "drop" doesn't record it at all, "keep" records it
    /// exactly as typed, for the user's own finder (agents still get it masked)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets: Option<String>,
    /// regexes: commands that are never recorded (e.g. "^curl ", "vault ")
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore_commands: Vec<String>,
    /// regexes: folders where nothing is recorded (e.g. "(?i)\\\\secret-project")
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore_folders: Vec<String>,
    /// strict permissions: only reman's own dialog box can let an agent see a folder; the AI
    /// app's approval buttons and dialogs can't (mcp.rs, Consent). For apps set to approve tool
    /// calls on their own. Where the dialog can't be shown (no desktop), nothing is shared.
    #[serde(default, alias = "consent_window", skip_serializing_if = "std::ops::Not::not")]
    pub strict_permissions: bool,
    /// a desktop notification when a command of yours that ran this many seconds finishes;
    /// absent = 60, 0 = never
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub done_alert_after_s: Option<u64>,
    /// the finder: how many lines it takes under the prompt; absent = 20, 0 = the whole screen
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finder_height: Option<u16>,
    /// the finder's keys: absent = as in a text box, "vim" = vim's normal and insert modes
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finder_keys: Option<String>,
    /// whose commands the finder lists at first: absent = yours, "all" = agents' too
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finder_who: Option<String>,
    /// what commands print is kept for this many runs, the newest (absent = 3000, 0 = none):
    /// agents' (their hooks see it) and yours inside `reman shell`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_runs: Option<u32>,
    /// shells open inside `reman shell` (so what your commands print is kept too)
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub shell_layer: bool,
    /// reman's keys: a preset (standard, gentle, vim; absent = standard) ...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_preset: Option<String>,
    /// ... and the actions the user changed: action -> keys (`"runs": ["Ctrl+E"]`; [] = off). See keys.rs
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub keys: std::collections::BTreeMap<String, Vec<String>>,
}

impl Settings {
    /// After how many seconds a finished command of yours gets a desktop notification; None = never.
    pub fn done_alert_after(&self) -> Option<u64> {
        match self.done_alert_after_s {
            None => Some(60),
            Some(0) => None,
            Some(n) => Some(n),
        }
    }

    /// Lines the finder takes under the prompt; None = the whole screen.
    pub fn finder_lines(&self) -> Option<u16> {
        match self.finder_height {
            None => Some(20),
            Some(0) => None,
            Some(n) => Some(n.max(8)),
        }
    }

    /// How many runs' output is kept, newest first; 0 = none.
    pub fn output_runs(&self) -> u32 {
        self.output_runs.unwrap_or(3000)
    }

    pub fn finder_vim(&self) -> bool {
        self.finder_keys.as_deref() == Some("vim")
    }

    /// The finder lists agents' commands too, from the start.
    pub fn finder_everyone(&self) -> bool {
        self.finder_who.as_deref() == Some("all")
    }

    /// Drop commands holding a secret instead of masking them.
    pub fn drop_secrets(&self) -> bool {
        self.secrets.as_deref() == Some("drop")
    }

    /// No redaction when recording: commands and error text are stored as typed, so the user's own
    /// finder shows them whole. Agents still get them masked (mcp::Policy::safe).
    pub fn keep_secrets(&self) -> bool {
        self.secrets.as_deref() == Some("keep")
    }

    /// "mask" | "drop" | "keep"
    pub fn secrets_mode(&self) -> &'static str {
        if self.drop_secrets() {
            "drop"
        } else if self.keep_secrets() {
            "keep"
        } else {
            "mask"
        }
    }
}

/// An OpenAI-compatible chat endpoint. Without `endpoint`, reman looks for a local server
/// (Ollama, LM Studio, llama.cpp); a remote one is only ever used when set here.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Ai {
    #[serde(default = "yes")]
    pub enabled: bool,
    /// e.g. `http://localhost:11434/v1`, `https://api.openai.com/v1`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// default: the endpoint's first chat model
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// the NAME of the environment variable holding the API key (default REMAN_AI_KEY); the key
    /// itself is never stored
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
}

fn yes() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Http {
    pub port: u16,
    pub token: String,
}

pub fn path() -> PathBuf {
    std::env::var_os("REMAN_CONFIG").map(PathBuf::from).unwrap_or_else(|| config::home().join("config.json"))
}

pub fn load() -> Settings {
    std::fs::read_to_string(path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Settings { strict_secrets: true, ..Default::default() })
}

pub fn save(s: &Settings) -> Result<()> {
    let p = path();
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(&p, serde_json::to_string_pretty(s)?).with_context(|| format!("writing {}", p.display()))
}

/// 256-bit bearer token from the OS-seeded SipHash keys (no extra crates, never logged).
pub fn new_token() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    (0..4)
        .map(|i| {
            let mut h = RandomState::new().build_hasher();
            h.write_u64(i);
            h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
            h.write_u32(std::process::id());
            format!("{:016x}", h.finish())
        })
        .collect()
}

pub fn roots_sep() -> &'static str {
    if cfg!(windows) { ";" } else { ":" }
}
