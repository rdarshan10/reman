//! `reman connect` - plug reman into AI agents with one command.
//!
//! Every agent gets the same stdio MCP server (`reman mcp`); the folder boundary lives in ONE
//! place (~/.reman/config.json `mcp_roots`), so `reman connect --root <dir>` re-scopes every agent
//! at once. Claude Code also gets the capture hooks (its Bash runs are recorded as agent:claude-code).
//! Configs are edited in place: invalid files are never overwritten, a .reman-bak copy is kept,
//! everything is idempotent, and `reman disconnect` removes exactly what was added.
use crate::settings;
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

pub const AGENTS: &[(&str, &str)] = &[
    ("claude-code", "Claude Code"),
    ("claude-desktop", "Claude Desktop"),
    ("codex", "OpenAI Codex CLI"),
    ("cursor", "Cursor"),
    ("vscode", "VS Code (Copilot agent mode)"),
    ("windsurf", "Windsurf"),
    ("gemini", "Gemini CLI"),
];

/// REMAN_CONNECT_HOME redirects every config path (tests / dry runs never touch real configs).
fn sandbox() -> Option<PathBuf> {
    std::env::var_os("REMAN_CONNECT_HOME").map(PathBuf::from)
}

fn home() -> PathBuf {
    sandbox().unwrap_or_else(|| dirs::home_dir().unwrap_or_default())
}

/// Per-user application config dir: %APPDATA% / ~/Library/Application Support / ~/.config
fn app_config() -> PathBuf {
    if let Some(s) = sandbox() {
        return s.join("appconfig");
    }
    dirs::config_dir().unwrap_or_else(|| home().join(".config"))
}

pub fn config_path(id: &str) -> Option<PathBuf> {
    Some(match id {
        "claude-code" => home().join(".claude.json"),
        "claude-desktop" => app_config().join("Claude").join("claude_desktop_config.json"),
        "codex" => std::env::var_os("CODEX_HOME").filter(|_| sandbox().is_none()).map(PathBuf::from).unwrap_or_else(|| home().join(".codex")).join("config.toml"),
        "cursor" => home().join(".cursor").join("mcp.json"),
        "vscode" => app_config().join("Code").join("User").join("mcp.json"),
        "windsurf" => home().join(".codeium").join("windsurf").join("mcp_config.json"),
        "gemini" => home().join(".gemini").join("settings.json"),
        _ => return None,
    })
}

fn claude_settings() -> PathBuf {
    home().join(".claude").join("settings.json")
}

fn on_path(name: &str) -> bool {
    let exts: &[&str] = if cfg!(windows) { &[".exe", ".cmd", ".bat", ""] } else { &[""] };
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| exts.iter().any(|e| d.join(format!("{name}{e}")).is_file())))
}

/// Is the agent installed? (its config dir exists, or its CLI is on PATH)
pub fn installed(id: &str) -> bool {
    let Some(p) = config_path(id) else { return false };
    let dir_ok = p.parent().is_some_and(|d| d.is_dir()) && (id != "claude-code" || p.exists() || home().join(".claude").is_dir());
    dir_ok
        || (sandbox().is_none()
            && match id {
                "claude-code" => on_path("claude"),
                "codex" => on_path("codex"),
                "gemini" => on_path("gemini"),
                "cursor" => on_path("cursor"),
                _ => false,
            })
}

fn entry(exe: &Path, vscode: bool) -> Value {
    let mut e = json!({"command": exe.to_string_lossy(), "args": ["mcp"]});
    if vscode {
        e["type"] = json!("stdio");
    }
    e
}

fn backup(p: &Path) -> Result<()> {
    if p.exists() {
        let b = PathBuf::from(format!("{}.reman-bak", p.display()));
        if !b.exists() {
            std::fs::copy(p, &b)?;
        }
    }
    Ok(())
}

/// Load a JSON config (missing/empty = {}), refusing to touch anything we can't parse.
fn load_json(p: &Path) -> Result<Map<String, Value>> {
    let text = match std::fs::read_to_string(p) {
        Ok(t) => t,
        Err(_) => return Ok(Map::new()),
    };
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => bail!("{} is not a JSON object - not touching it", p.display()),
        Err(e) => bail!("{} isn't plain JSON ({e}) - not touching it; add the entry by hand (reman connect --print)", p.display()),
    }
}

fn save_json(p: &Path, m: Map<String, Value>) -> Result<()> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    backup(p)?;
    std::fs::write(p, serde_json::to_string_pretty(&Value::Object(m))? + "\n").with_context(|| format!("writing {}", p.display()))
}

fn toml_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Drop `[mcp_servers.reman]` (and its sub-tables) from a TOML document, keeping everything else.
fn toml_without_reman(text: &str) -> String {
    let mut out = Vec::new();
    let mut skipping = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            let name = t.trim_start_matches('[').trim_end_matches(']').trim();
            skipping = name == "mcp_servers.reman" || name.starts_with("mcp_servers.reman.");
        }
        if !skipping {
            out.push(line);
        }
    }
    while out.last().is_some_and(|l| l.trim().is_empty()) {
        out.pop();
    }
    out.join("\n")
}

fn codex_block(exe: &Path) -> String {
    format!("[mcp_servers.reman]\ncommand = {}\nargs = [\"mcp\"]\n", toml_str(&exe.to_string_lossy()))
}

fn mcp_key(id: &str) -> &'static str {
    if id == "vscode" { "servers" } else { "mcpServers" }
}

/// Current state of one agent: None = not connected, Some(path_matches)
pub fn status(id: &str, exe: &Path) -> Option<bool> {
    let p = config_path(id)?;
    if id == "codex" {
        let t = std::fs::read_to_string(&p).ok()?;
        return t.contains("[mcp_servers.reman]").then(|| t.contains(&toml_str(&exe.to_string_lossy())));
    }
    let m = load_json(&p).ok()?;
    let e = m.get(mcp_key(id))?.get("reman")?;
    Some(e.get("command").and_then(Value::as_str).is_some_and(|c| Path::new(c) == exe))
}

fn claude_cli_available() -> bool {
    sandbox().is_none() && on_path("claude")
}

fn run_claude(args: &[&str]) -> Result<std::process::Output> {
    let exe = if cfg!(windows) && !on_path_exact("claude.exe") && on_path_exact("claude.cmd") { "claude.cmd" } else { "claude" };
    Ok(std::process::Command::new(exe).args(args).output()?)
}

fn on_path_exact(file: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(file).is_file()))
}

pub fn connect(id: &str, exe: &Path, hook: &Path) -> Result<String> {
    let p = config_path(id).context("unknown agent")?;
    match id {
        "claude-code" => {
            if claude_cli_available() {
                // the official CLI edits ~/.claude.json atomically (Claude Code rewrites it constantly)
                let _ = run_claude(&["mcp", "remove", "-s", "user", "reman"]);
                let exe_s = exe.to_string_lossy().into_owned();
                let out = run_claude(&["mcp", "add", "-s", "user", "reman", "--", &exe_s, "mcp"])?;
                if !out.status.success() {
                    bail!("claude mcp add failed: {}", String::from_utf8_lossy(&out.stderr).trim());
                }
            } else {
                let mut m = load_json(&p)?;
                let servers = m.entry("mcpServers").or_insert_with(|| json!({}));
                servers["reman"] = json!({"type": "stdio", "command": exe.to_string_lossy(), "args": ["mcp"], "env": {}});
                save_json(&p, m)?;
            }
            set_claude_hooks(Some(hook))?;
            Ok(format!("MCP server (user scope) + capture hooks in {}", claude_settings().display()))
        }
        "codex" => {
            let text = std::fs::read_to_string(&p).unwrap_or_default();
            let mut new = toml_without_reman(&text);
            if !new.is_empty() {
                new.push_str("\n\n");
            }
            new.push_str(&codex_block(exe));
            if let Some(d) = p.parent() {
                std::fs::create_dir_all(d)?;
            }
            backup(&p)?;
            std::fs::write(&p, new)?;
            Ok(format!("[mcp_servers.reman] in {}", p.display()))
        }
        _ => {
            let mut m = load_json(&p)?;
            let key = mcp_key(id);
            let servers = m.entry(key).or_insert_with(|| json!({}));
            if !servers.is_object() {
                bail!("{}: `{key}` is not an object - not touching it", p.display());
            }
            servers["reman"] = entry(exe, id == "vscode");
            save_json(&p, m)?;
            Ok(format!("\"{key}\".reman in {}", p.display()))
        }
    }
}

pub fn disconnect(id: &str) -> Result<String> {
    let p = config_path(id).context("unknown agent")?;
    match id {
        "claude-code" => {
            if claude_cli_available() {
                let _ = run_claude(&["mcp", "remove", "-s", "user", "reman"]);
            } else if p.exists() {
                let mut m = load_json(&p)?;
                if let Some(s) = m.get_mut("mcpServers").and_then(Value::as_object_mut) {
                    s.remove("reman");
                }
                save_json(&p, m)?;
            }
            set_claude_hooks(None)?;
            Ok("removed MCP server + capture hooks".into())
        }
        "codex" => {
            if let Ok(t) = std::fs::read_to_string(&p) {
                backup(&p)?;
                std::fs::write(&p, toml_without_reman(&t) + "\n")?;
            }
            Ok(format!("removed from {}", p.display()))
        }
        _ => {
            if p.exists() {
                let mut m = load_json(&p)?;
                if let Some(s) = m.get_mut(mcp_key(id)).and_then(Value::as_object_mut) {
                    s.remove("reman");
                }
                save_json(&p, m)?;
            }
            Ok(format!("removed from {}", p.display()))
        }
    }
}

/// Claude Code PostToolUse/PostToolUseFailure hooks -> reman-hook claude (None = remove ours).
fn set_claude_hooks(hook: Option<&Path>) -> Result<()> {
    let p = claude_settings();
    let mut m = load_json(&p)?;
    let hooks = m.entry("hooks").or_insert_with(|| json!({}));
    for ev in ["PostToolUse", "PostToolUseFailure"] {
        let list = hooks.as_object_mut().context("settings.json `hooks` is not an object")?.entry(ev).or_insert_with(|| json!([]));
        let arr = list.as_array_mut().context("hook list is not an array")?;
        // drop any previous reman hook, keep everything else the user has
        for block in arr.iter_mut() {
            if let Some(hs) = block.get_mut("hooks").and_then(Value::as_array_mut) {
                hs.retain(|h| !h.get("command").and_then(Value::as_str).is_some_and(|c| c.contains("reman")));
            }
        }
        arr.retain(|b| b.get("hooks").and_then(Value::as_array).is_none_or(|h| !h.is_empty()));
        if let Some(h) = hook {
            arr.push(json!({"matcher": "Bash|PowerShell", "hooks": [{"type": "command", "command": h.to_string_lossy().replace('\\', "/"), "args": ["claude"], "timeout": 5}]}));
        }
    }
    // leave no empty scaffolding behind on disconnect
    if let Some(obj) = hooks.as_object_mut() {
        obj.retain(|_, v| v.as_array().is_none_or(|a| !a.is_empty()));
    }
    if hooks.as_object().is_some_and(|o| o.is_empty()) {
        m.remove("hooks");
    }
    save_json(&p, m)
}

/// Roots precedence: --root flags, else config.json, else the old Claude Code env, else cwd.
pub fn resolve_roots(cli: &[String]) -> Vec<String> {
    let abs = |r: &str| std::path::absolute(r).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| r.to_string());
    if !cli.is_empty() {
        return cli.iter().map(|r| abs(r)).collect();
    }
    let st = settings::load();
    if !st.mcp_roots.is_empty() {
        return st.mcp_roots;
    }
    let legacy = load_json(&home().join(".claude.json"))
        .ok()
        .and_then(|m| m.get("mcpServers")?.get("reman")?.get("env")?.get("REMAN_MCP_ROOT")?.as_str().map(str::to_string));
    if let Some(l) = legacy {
        return l.split(settings::roots_sep()).filter(|s| !s.trim().is_empty()).map(str::to_string).collect();
    }
    // nothing chosen: no shared list. Each agent then sees only the project it was started in
    // (mcp::Policy::from_env). Never the folder `connect` happened to run from - for a new user
    // that is their home folder, i.e. everything.
    Vec::new()
}

/// One line saying what agents can see, for `reman connect` output.
pub fn roots_line(roots: &[String]) -> String {
    if roots.is_empty() {
        "each agent sees only the project folder it's working in (reman connect --add-root <dir> to share more)".into()
    } else {
        roots.join("  |  ")
    }
}

/// Paste-able config for agents we don't know about.
pub fn generic_snippet(exe: &Path) -> String {
    let e = exe.to_string_lossy();
    format!(
        "Any MCP client (stdio):\n  {{\"mcpServers\": {{\"reman\": {{\"command\": {}, \"args\": [\"mcp\"]}}}}}}\n\nOpenAI Codex (~/.codex/config.toml):\n{}",
        serde_json::to_string(&e).unwrap_or_default(),
        codex_block(exe)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_edit_is_surgical() {
        let t = "model = \"o3\"\n\n[mcp_servers.other]\ncommand = \"x\"\n\n[mcp_servers.reman]\ncommand = \"old\"\n[mcp_servers.reman.env]\nA = \"1\"\n\n[profile]\nx = 1\n";
        let out = toml_without_reman(t);
        assert!(out.contains("[mcp_servers.other]") && out.contains("[profile]") && out.contains("model = \"o3\""));
        assert!(!out.contains("mcp_servers.reman") && !out.contains("old"));
        assert_eq!(toml_str(r#"C:\a "b""#), r#""C:\\a \"b\"""#);
    }
}
