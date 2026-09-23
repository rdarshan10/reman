//! Capture path - replaces Atuin. Shell hooks and agent hooks call `reman record` / `reman hook`,
//! which push one run to the daemon in a few ms. If the daemon is down the run is appended to the
//! spool file (never lost) and the daemon is started in the background to drain it.
use crate::client::{self, Client};
use crate::config;
use anyhow::Result;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::time::Duration;

/// ActorDetector: $AGENT convention first, then per-agent env markers (data, not logic).
pub fn detect_actor() -> String {
    const MARKERS: &[(&str, &[&str])] = &[("claude-code", &["CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT"]), ("codex", &["CODEX_CI"])];
    if let Ok(a) = std::env::var("AGENT") {
        if !a.trim().is_empty() {
            return format!("agent:{}", a.trim().to_lowercase());
        }
    }
    for (name, ms) in MARKERS {
        if ms.iter().any(|m| std::env::var_os(m).is_some_and(|v| !v.is_empty())) {
            return format!("agent:{name}");
        }
    }
    "human".into()
}

/// Commands the user asked us never to keep: leading space (HISTCONTROL=ignorespace habit) or a
/// match of $REMAN_IGNORE (regex).
pub fn ignored(cmd: &str) -> bool {
    if cmd.starts_with(' ') || cmd.trim().is_empty() {
        return true;
    }
    if let Ok(p) = std::env::var("REMAN_IGNORE") {
        if let Ok(rx) = regex::Regex::new(&p) {
            return rx.is_match(cmd);
        }
    }
    false
}

pub fn spool(req: &Value) -> Result<()> {
    std::fs::create_dir_all(config::home())?;
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(config::spool_path())?;
    let mut line = serde_json::to_vec(req)?;
    line.push(b'\n');
    f.write_all(&line)?; // single write: appends from concurrent shells don't interleave
    Ok(())
}

/// Push one ingest; returns the daemon's suggestion for a failed command when `want_reply`.
pub fn push(mut req: Value, want_reply: bool) -> Option<Value> {
    if req.get("ts").is_none() {
        req["ts"] = json!(config::now());
    }
    match Client::connect_timeout(Duration::from_millis(250)) {
        Ok(mut c) => {
            if want_reply {
                c.set_timeout(Some(Duration::from_millis(1500)));
                c.call(&req).ok().and_then(|v| v.get("suggest").cloned())
            } else {
                let _ = c.send(&req);
                None
            }
        }
        Err(_) => {
            let _ = spool(&req);
            let _ = client::spawn_daemon();
            None
        }
    }
}

pub struct RecordArgs {
    pub command: String,
    pub exit: Option<i64>,
    pub cwd: Option<String>,
    pub session: Option<String>,
    pub actor: Option<String>,
    pub duration_ms: Option<i64>,
    pub suggest: bool,
    /// also print the bare fix command on stdout (for shells that capture it, e.g. fish Alt-F)
    pub print_fix: bool,
}

pub fn record(a: RecordArgs) -> Result<()> {
    if ignored(&a.command) {
        return Ok(());
    }
    let cwd = a.cwd.or_else(|| std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned()));
    let req = json!({
        "op": "ingest", "command": a.command.trim(), "exit": a.exit, "cwd": cwd,
        "session": a.session.or_else(|| std::env::var("REMAN_SESSION").ok()).unwrap_or_default(),
        "actor": a.actor.unwrap_or_else(detect_actor), "duration_ms": a.duration_ms,
    });
    let failed = a.exit.is_some_and(|e| e > 0);
    if let Some(sg) = push(req, a.suggest && failed) {
        print_suggestion(&sg);
        if a.print_fix {
            if let Some(c) = sg.get("command").and_then(Value::as_str) {
                println!("{c}");
            }
        }
    }
    Ok(())
}

pub fn print_suggestion(sg: &Value) {
    let Some(cmd) = sg.get("command").and_then(Value::as_str) else { return };
    let lead = if sg.get("kind").and_then(Value::as_str) == Some("proven") { "last time this failed you ran" } else { "did you mean" };
    eprintln!("\x1b[90m  reman: {lead} \u{2192} \x1b[36m{cmd}\x1b[0m");
}

/// Claude Code PostToolUse / PostToolUseFailure hook: payload JSON on stdin.
pub fn hook_claude() -> Result<()> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    let Ok(data) = serde_json::from_str::<Value>(&buf) else { return Ok(()) };
    if data.get("tool_name").and_then(Value::as_str) != Some("Bash") {
        return Ok(());
    }
    let cmd = data.pointer("/tool_input/command").and_then(Value::as_str).unwrap_or("").trim().to_string();
    if cmd.is_empty() {
        return Ok(());
    }
    let cwd = data.get("cwd").and_then(Value::as_str).map(str::to_string).or_else(|| std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned()));
    let session = data.get("session_id").and_then(Value::as_str).map(str::to_string).or_else(|| std::env::var("CLAUDE_CODE_SESSION_ID").ok()).unwrap_or_default();
    let event = data.get("hook_event_name").and_then(Value::as_str).unwrap_or("");
    let resp = data.get("tool_response").cloned().unwrap_or(Value::Null);
    let mut exit: i64 = ["exit_code", "exitCode", "returncode", "code"].iter().find_map(|k| resp.get(*k).and_then(Value::as_i64)).unwrap_or(0);
    let flag = |k: &str| resp.get(k).and_then(Value::as_bool).unwrap_or(false);
    if (flag("is_error") || flag("interrupted")) && exit == 0 {
        exit = 1;
    }
    // a failed Bash command fires PostToolUseFailure, not PostToolUse
    if event == "PostToolUseFailure" && exit == 0 {
        exit = 1;
    }
    push(json!({"op": "ingest", "command": cmd, "exit": exit, "cwd": cwd, "session": session, "actor": "agent:claude-code"}), false);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignore_rules() {
        assert!(ignored(" secret-thing"));
        assert!(ignored("   "));
        assert!(!ignored("git status"));
    }

    #[test]
    fn spool_roundtrip() {
        let dir = std::env::temp_dir().join(format!("reman-spool-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("spool.jsonl");
        unsafe { std::env::set_var("REMAN_SPOOL", &p) };
        spool(&json!({"op": "ingest", "command": "a b", "exit": 0})).unwrap();
        spool(&json!({"op": "ingest", "command": "c d", "exit": 1})).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        let runs: Vec<Value> = text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).collect();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[1]["exit"], json!(1));
        assert_eq!(runs[0]["command"], json!("a b"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
