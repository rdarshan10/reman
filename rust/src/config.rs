//! Paths + tunables shared by every subcommand. Everything lives under ~/.reman unless overridden.
use std::path::PathBuf;

pub const DIM: usize = 384;
pub const HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 8765;
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn home() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")).join(".reman")
}

/// REMAN_DB overrides (tests point this at a temp file), same env var the Python code honoured.
pub fn db_path() -> PathBuf {
    std::env::var_os("REMAN_DB").map(PathBuf::from).unwrap_or_else(|| home().join("reman.db"))
}

pub fn port() -> u16 {
    std::env::var("REMAN_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(DEFAULT_PORT)
}

/// Commands captured while the daemon is down are appended here and drained on the next start.
pub fn spool_path() -> PathBuf {
    std::env::var_os("REMAN_SPOOL").map(PathBuf::from).unwrap_or_else(|| home().join("spool.jsonl"))
}

pub fn models_dir() -> PathBuf {
    home().join("models")
}

pub fn log_path() -> PathBuf {
    home().join("daemon.log")
}

pub fn bin_dir() -> PathBuf {
    home().join("bin")
}

pub fn atuin_db_path() -> Option<PathBuf> {
    let mut cands = Vec::new();
    if let Some(x) = std::env::var_os("XDG_DATA_HOME") {
        cands.push(PathBuf::from(x).join("atuin").join("history.db"));
    }
    if let Some(h) = dirs::home_dir() {
        cands.push(h.join(".local").join("share").join("atuin").join("history.db"));
    }
    if let Some(la) = std::env::var_os("LOCALAPPDATA") {
        cands.push(PathBuf::from(la).join("atuin").join("history.db"));
    }
    cands.into_iter().find(|p| p.exists())
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// "today" / "3d ago" - the age label every surface (CLI, TUI, MCP) shows.
pub fn age(ts: i64) -> String {
    if ts <= 0 {
        return "?".into();
    }
    let d = (now() - ts) as f64 / 86400.0;
    if d < 1.0 { "today".into() } else { format!("{}d ago", d as i64) }
}

/// Folder equality the way Python's os.path.normcase(normpath()) did it: case-insensitive and
/// separator-agnostic on Windows, trailing separators ignored.
pub fn norm_path(p: &str) -> String {
    let mut s = p.trim().replace('/', std::path::MAIN_SEPARATOR_STR);
    while s.len() > 1 && s.ends_with(std::path::MAIN_SEPARATOR) && !s.ends_with(":\\") {
        s.pop();
    }
    if cfg!(windows) { s.to_lowercase() } else { s }
}
