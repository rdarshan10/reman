//! `reman connect` - plug reman into AI agents with one command.
//!
//! Every agent gets the same stdio MCP server (`reman mcp`); the folder boundary lives in ONE
//! place (~/.reman/config.json `mcp_roots`), so `reman connect --root <dir>` re-scopes every agent
//! at once. Every agent with a shell tool also gets capture: its commands are recorded as
//! agent:<name>, through the agent's own hooks (Claude Code, Codex, Cursor, VS Code, Windsurf,
//! Gemini CLI) or a plugin of reman's (opencode, pi, which have no JSON hooks).
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
    ("opencode", "opencode"),
    ("pi", "pi"),
];

/// Where each agent tells hooks about the shell commands it runs, so reman records them (as
/// agent:<name>): every agent with a shell tool. Claude Desktop has none.
fn captures(id: &str) -> bool {
    id != "claude-desktop"
}

/// REMAN_CONNECT_HOME redirects every config path (tests / dry runs never touch real configs).
pub(crate) fn sandbox() -> Option<PathBuf> {
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
        "opencode" => xdg_config().join("opencode").join("opencode.json"),
        // pi has no MCP: its config is reman's extension
        "pi" => pi_dir().join("extensions").join("reman.ts"),
        _ => return None,
    })
}

/// $XDG_CONFIG_HOME, else ~/.config (opencode's config lives there on every OS)
fn xdg_config() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME").filter(|_| sandbox().is_none()).map(PathBuf::from).unwrap_or_else(|| home().join(".config"))
}

/// $PI_CODING_AGENT_DIR, else ~/.pi/agent
fn pi_dir() -> PathBuf {
    std::env::var_os("PI_CODING_AGENT_DIR").filter(|_| sandbox().is_none()).map(PathBuf::from).unwrap_or_else(|| home().join(".pi").join("agent"))
}

fn opencode_plugin() -> PathBuf {
    xdg_config().join("opencode").join("plugins").join("reman.ts")
}

/// VS Code reads the user's hook files from ~/.copilot/hooks ($COPILOT_HOME/hooks), as does the
/// Copilot CLI; this one is all reman's.
fn copilot_hooks() -> PathBuf {
    let dir = std::env::var_os("COPILOT_HOME").filter(|_| sandbox().is_none()).map(PathBuf::from).unwrap_or_else(|| home().join(".copilot"));
    dir.join("hooks").join("reman.json")
}

fn cursor_hooks() -> PathBuf {
    home().join(".cursor").join("hooks.json")
}

fn windsurf_hooks() -> PathBuf {
    home().join(".codeium").join("windsurf").join("hooks.json")
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
    let dir_ok = match id {
        "pi" => pi_dir().is_dir(),
        _ => p.parent().is_some_and(|d| d.is_dir()) && (id != "claude-code" || p.exists() || home().join(".claude").is_dir()),
    };
    dir_ok
        || (sandbox().is_none()
            && match id {
                "claude-code" => on_path("claude"),
                "codex" => on_path("codex"),
                "gemini" => on_path("gemini"),
                "cursor" => on_path("cursor"),
                "opencode" => on_path("opencode"),
                "pi" => on_path("pi"),
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
    match id {
        "codex" => {
            let t = std::fs::read_to_string(&p).ok()?;
            t.contains("[mcp_servers.reman]").then(|| t.contains(&toml_str(&exe.to_string_lossy())))
        }
        // reman's own extension, calling the reman-hook next to this reman
        "pi" => std::fs::read_to_string(&p).ok().map(|t| t.contains(&fwd(&hook_for(exe)))),
        "opencode" => {
            let mcp = load_json(&p).ok().and_then(|m| m.get("mcp")?.get("reman")?.pointer("/command/0")?.as_str().map(|c| Path::new(c) == exe));
            mcp.or_else(|| std::fs::read_to_string(opencode_plugin()).ok().map(|t| t.contains(&fwd(&hook_for(exe)))))
        }
        _ => {
            let m = load_json(&p).ok()?;
            let e = m.get(mcp_key(id))?.get("reman")?;
            Some(e.get("command").and_then(Value::as_str).is_some_and(|c| Path::new(c) == exe))
        }
    }
}

/// The reman-hook installed next to `exe`.
fn hook_for(exe: &Path) -> PathBuf {
    exe.with_file_name(if cfg!(windows) { "reman-hook.exe" } else { "reman-hook" })
}

/// A path with forward slashes: valid on every OS, and no escaping in JSON, TOML or TypeScript.
fn fwd(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// `<reman-hook> <agent>` as one command line a shell runs: quoted only when it must be (a bare
/// path runs the same in sh, cmd and PowerShell; a quoted one is only a string to PowerShell).
fn cmdline(hook: &Path, agent: &str) -> String {
    let exe = fwd(hook);
    if exe.contains(char::is_whitespace) { format!("\"{exe}\" {agent}") } else { format!("{exe} {agent}") }
}

/// Uninstall: reman out of every coding tool's config (its MCP entry, its capture hooks), and
/// out of hook files left without an entry. What it did, or would do when `dry`.
pub fn remove_everywhere(exe: &Path, dry: bool) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for (id, name) in AGENTS {
        if status(id, exe).is_some() {
            if !dry {
                disconnect(id)?;
            }
            let what = if *id == "pi" { "reman's extension" } else if captures(id) { "reman's MCP server and its capture hooks" } else { "reman's MCP server" };
            out.push(format!("{name}: {what}"));
        } else if captures(id) && capture_installed(id) {
            // hooks left behind without the MCP entry
            if !dry {
                set_capture(id, None)?;
            }
            out.push(format!("{name}: reman's capture hooks"));
        }
    }
    Ok(out)
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
            let hooks = set_capture(id, Some(hook))?;
            Ok(format!("MCP server (user scope) + capture hooks in {}", hooks.display()))
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
            let hooks = set_capture(id, Some(hook))?;
            Ok(format!("[mcp_servers.reman] in {} + capture hooks in {} (approve them once in Codex: /hooks)", p.display(), hooks.display()))
        }
        "opencode" => {
            let plugin = set_capture(id, Some(hook))?;
            // opencode.json may be JSONC: then the MCP entry is the user's to add, the plugin still records
            let mcp = load_json(&p).and_then(|mut m| {
                let servers = m.entry("mcp").or_insert_with(|| json!({}));
                if !servers.is_object() {
                    bail!("{}: `mcp` is not an object - not touching it", p.display());
                }
                servers["reman"] = json!({"type": "local", "command": [exe.to_string_lossy(), "mcp"], "enabled": true});
                save_json(&p, m)
            });
            match mcp {
                Ok(()) => Ok(format!("\"mcp\".reman in {} + capture plugin {}", p.display(), plugin.display())),
                Err(e) => Ok(format!("capture plugin {} (MCP not added: {e:#})", plugin.display())),
            }
        }
        "pi" => {
            let ext = set_capture(id, Some(hook))?;
            Ok(format!("capture extension {} (pi has no MCP; restart pi or /reload)", ext.display()))
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
            if captures(id) {
                let hooks = set_capture(id, Some(hook))?;
                return Ok(format!("\"{key}\".reman in {} + capture hooks in {}", p.display(), hooks.display()));
            }
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
            set_capture(id, None)?;
            Ok("removed MCP server + capture hooks".into())
        }
        "codex" => {
            if let Ok(t) = std::fs::read_to_string(&p) {
                backup(&p)?;
                std::fs::write(&p, toml_without_reman(&t) + "\n")?;
            }
            set_capture(id, None)?;
            Ok(format!("removed from {} and its capture hooks", p.display()))
        }
        "pi" => {
            set_capture(id, None)?;
            Ok(format!("removed {}", p.display()))
        }
        _ => {
            if p.exists() {
                // (an opencode.json that isn't plain JSON never got an entry)
                if let Ok(mut m) = load_json(&p) {
                    let key = if id == "opencode" { "mcp" } else { mcp_key(id) };
                    if let Some(s) = m.get_mut(key).and_then(Value::as_object_mut) {
                        s.remove("reman");
                    }
                    save_json(&p, m)?;
                }
            }
            if captures(id) {
                set_capture(id, None)?;
                return Ok(format!("removed from {} and its capture hooks", p.display()));
            }
            Ok(format!("removed from {}", p.display()))
        }
    }
}

/// Codex reads hooks from hooks.json next to its config.toml.
fn codex_hooks_path() -> Option<PathBuf> {
    config_path("codex").and_then(|p| p.parent().map(|d| d.join("hooks.json")))
}

/// The file an agent's capture lives in.
fn capture_file(id: &str) -> Option<PathBuf> {
    Some(match id {
        "claude-code" => claude_settings(),
        "codex" => codex_hooks_path()?,
        "cursor" => cursor_hooks(),
        "vscode" => copilot_hooks(),
        "windsurf" => windsurf_hooks(),
        "gemini" => config_path("gemini")?,
        "opencode" => opencode_plugin(),
        "pi" => config_path("pi")?,
        _ => return None,
    })
}

/// Are reman's capture hooks there (reman-hook named in the agent's hook file)?
pub fn capture_installed(id: &str) -> bool {
    capture_file(id).and_then(|p| std::fs::read_to_string(p).ok()).is_some_and(|t| t.contains("reman-hook"))
}

/// Install (Some(reman-hook)) or remove (None) an agent's capture, in the way that agent takes
/// it: JSON hooks (Claude Code, Codex, Cursor, VS Code, Windsurf, Gemini CLI) or reman's own
/// plugin (opencode, pi, which have no JSON hooks). Everything else the user has stays. The
/// file touched.
fn set_capture(id: &str, hook: Option<&Path>) -> Result<PathBuf> {
    let p = capture_file(id).context("this agent takes no capture hooks")?;
    match id {
        // PreToolUse marks a start (for the duration), PostToolUse / PostToolUseFailure record
        "claude-code" => {
            let e = hook.map(|h| json!({"type": "command", "command": fwd(h), "args": ["claude"], "timeout": 5}));
            set_nested_hooks(&p, &["PreToolUse", "PostToolUse", "PostToolUseFailure"], "^(Bash|PowerShell)$", e)?;
        }
        "codex" => {
            let e = hook.map(|h| json!({"type": "command", "command": format!("\"{}\" codex", fwd(h)), "timeout": 5}));
            set_nested_hooks(&p, &["PreToolUse", "PostToolUse", "PostToolUseFailure"], "^(Bash|shell|local_shell)$", e)?;
        }
        // Gemini CLI: AfterTool on its shell tool (timeout in ms)
        "gemini" => {
            let e = hook.map(|h| json!({"type": "command", "command": cmdline(h, "gemini"), "name": "reman", "timeout": 5000}));
            set_nested_hooks(&p, &["AfterTool"], "run_shell_command", e)?;
        }
        // Cursor: afterShellExecution carries the output and the duration
        "cursor" => {
            let e = hook.map(|h| json!({"command": cmdline(h, "cursor"), "timeout": 5}));
            set_flat_hooks(&p, &["afterShellExecution"], e, Some(json!(1)))?;
        }
        // Windsurf: pre_run_command marks a start, post_run_command records
        "windsurf" => {
            let e = hook.map(|h| json!({"command": cmdline(h, "windsurf"), "powershell": format!("& '{}' windsurf", fwd(h)), "show_output": false}));
            set_flat_hooks(&p, &["pre_run_command", "post_run_command"], e, None)?;
        }
        // VS Code (Copilot agent mode): a hook file of reman's own in ~/.copilot/hooks. The Copilot
        // CLI reads that folder too, so the file is valid for both: `version` (the CLI's), PascalCase
        // events (VS Code's; the CLI then speaks VS Code's snake_case payload), the command for
        // each (`command`/`windows` for VS Code, `bash`/`powershell` for the CLI), both timeouts.
        // The matcher narrows it to their terminal tools where it's honoured.
        "vscode" => match hook {
            Some(h) => {
                let c = cmdline(h, "copilot");
                let e = json!({"type": "command", "matcher": "run_in_terminal|bash|powershell", "command": c, "windows": c, "bash": c, "powershell": c, "timeout": 5, "timeoutSec": 5});
                save_json(&p, json!({"version": 1, "hooks": {"PreToolUse": [e.clone()], "PostToolUse": [e]}}).as_object().cloned().unwrap_or_default())?;
            }
            None => remove_ours(&p)?,
        },
        "opencode" | "pi" => match hook {
            Some(h) => {
                let src = if id == "pi" { include_str!("init/pi.ts") } else { include_str!("init/opencode.ts") };
                let js = serde_json::to_string(&fwd(h))?;
                if let Some(d) = p.parent() {
                    std::fs::create_dir_all(d)?;
                }
                std::fs::write(&p, src.replace("\"@@REMAN_HOOK@@\"", &js)).with_context(|| format!("writing {}", p.display()))?;
            }
            None => remove_ours(&p)?,
        },
        _ => bail!("this agent takes no capture hooks"),
    }
    Ok(p)
}

/// Delete a file only reman writes, when it is reman's.
fn remove_ours(p: &Path) -> Result<()> {
    if std::fs::read_to_string(p).is_ok_and(|t| t.contains("reman")) {
        std::fs::remove_file(p)?;
    }
    Ok(())
}

fn is_ours(v: &Value) -> bool {
    ["command", "powershell", "windows", "bash"].iter().any(|k| v.get(*k).and_then(Value::as_str).is_some_and(|c| c.contains("reman-hook") || c.contains("reman.exe")))
}

/// Hooks as `{"hooks": {Event: [{"matcher": .., "hooks": [entry]}]}}` (Claude Code, Codex,
/// Gemini CLI): ours replaced by `entry` (None: removed), everything else the user has kept.
fn set_nested_hooks(p: &Path, events: &[&str], matcher: &str, entry: Option<Value>) -> Result<()> {
    let mut m = load_json(p)?;
    let hooks = m.entry("hooks").or_insert_with(|| json!({}));
    for ev in events {
        let list = hooks.as_object_mut().context("`hooks` is not an object")?.entry(*ev).or_insert_with(|| json!([]));
        let arr = list.as_array_mut().context("hook list is not an array")?;
        for block in arr.iter_mut() {
            if let Some(hs) = block.get_mut("hooks").and_then(Value::as_array_mut) {
                hs.retain(|h| !is_ours(h));
            }
        }
        arr.retain(|b| b.get("hooks").and_then(Value::as_array).is_none_or(|h| !h.is_empty()));
        if let Some(e) = &entry {
            arr.push(json!({"matcher": matcher, "hooks": [e]}));
        }
    }
    finish_hooks(p, m, &[])
}

/// Hooks as `{"hooks": {event: [entry]}}` (Cursor, Windsurf); Cursor's file also says its
/// `version`.
fn set_flat_hooks(p: &Path, events: &[&str], entry: Option<Value>, version: Option<Value>) -> Result<()> {
    if entry.is_none() && !p.exists() {
        return Ok(());
    }
    let mut m = load_json(p)?;
    if let (Some(v), Some(_)) = (&version, &entry) {
        m.entry("version").or_insert_with(|| v.clone());
    }
    let hooks = m.entry("hooks").or_insert_with(|| json!({}));
    for ev in events {
        let list = hooks.as_object_mut().context("`hooks` is not an object")?.entry(*ev).or_insert_with(|| json!([]));
        let arr = list.as_array_mut().context("hook list is not an array")?;
        arr.retain(|h| !is_ours(h));
        if let Some(e) = &entry {
            arr.push(e.clone());
        }
    }
    finish_hooks(p, m, &["version"])
}

/// Save a hook file without empty scaffolding; a file left holding nothing of the user's (only
/// `boilerplate` keys) is deleted.
fn finish_hooks(p: &Path, mut m: Map<String, Value>, boilerplate: &[&str]) -> Result<()> {
    if let Some(obj) = m.get_mut("hooks").and_then(Value::as_object_mut) {
        obj.retain(|_, v| v.as_array().is_none_or(|a| !a.is_empty()));
    }
    if m.get("hooks").and_then(Value::as_object).is_some_and(|o| o.is_empty()) {
        m.remove("hooks");
    }
    if !boilerplate.is_empty() && m.keys().all(|k| boilerplate.contains(&k.as_str())) {
        if p.exists() {
            std::fs::remove_file(p)?;
        }
        return Ok(());
    }
    save_json(p, m)
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
    // nothing chosen: no shared list, and agents see nothing until the user approves a folder.
    // Never the folder `connect` happened to run from - for a new user that is their home folder,
    // i.e. everything.
    Vec::new()
}

/// The coding agent running this process, if any: sharing a folder is the user's approval, so an
/// agent must not be able to grant it to itself.
pub fn run_by_agent() -> Option<&'static str> {
    let set = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty() && v != "0");
    if set("CLAUDECODE") {
        Some("Claude Code")
    } else if set("GEMINI_CLI") {
        Some("Gemini CLI")
    } else if std::env::vars_os().any(|(k, _)| k.to_string_lossy().starts_with("CODEX_")) {
        Some("Codex")
    } else {
        None
    }
}

pub fn refuse_if_agent(what: &str) -> anyhow::Result<()> {
    if let Some(a) = run_by_agent() {
        anyhow::bail!("{what} needs the user's approval, and this is running under {a}. Run it in your own terminal.");
    }
    Ok(())
}

/// One line saying what agents can see, for `reman connect` output.
pub fn roots_line(roots: &[String]) -> String {
    if roots.is_empty() {
        "nothing yet: agents see no history until you approve a folder (reman connect --add-root <dir>)".into()
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
