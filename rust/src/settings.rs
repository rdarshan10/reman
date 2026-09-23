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
