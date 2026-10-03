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
    const MARKERS: &[(&str, &[&str])] = &[
        ("claude-code", &["CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT"]),
        ("codex", &["CODEX_CI"]),
        ("gemini-cli", &["GEMINI_CLI"]),
        ("opencode", &["OPENCODE"]),
    ];
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

/// Push one ingest; returns the daemon's reply (the suggestion and note for a failed command)
/// when `want_reply`.
pub fn push(mut req: Value, want_reply: bool) -> Option<Value> {
    if req.get("ts").is_none() {
        req["ts"] = json!(config::now());
    }
    match Client::connect_timeout(Duration::from_millis(250)) {
        Ok(mut c) => {
            if want_reply {
                c.set_timeout(Some(Duration::from_millis(1500)));
                c.call(&req).ok()
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
    let mut req = json!({
        "op": "ingest", "command": a.command.trim(), "exit": a.exit, "cwd": cwd,
        "session": a.session.or_else(|| std::env::var("REMAN_SESSION").ok()).unwrap_or_default(),
        "actor": a.actor.unwrap_or_else(detect_actor), "duration_ms": a.duration_ms,
    });
    // inside `reman shell`: the id its output arrives under
    if let Some(c) = std::env::var("REMAN_CAPTURE").ok().filter(|c| !c.is_empty()) {
        req["capture"] = json!(c);
    }
    let failed = a.exit.is_some_and(|e| e > 0);
    if let Some(reply) = push(req, a.suggest && failed) {
        if let Some(sg) = reply.get("suggest") {
            print_suggestion(sg);
            if a.print_fix {
                if let Some(c) = sg.get("command").and_then(Value::as_str) {
                    println!("{c}");
                }
            }
        }
        print_note(&reply);
    }
    Ok(())
}

/// The daemon's one-line note (what broke a command that used to work here; last time here).
pub fn print_note(reply: &Value) {
    for k in ["note", "line"] {
        if let Some(n) = reply.get(k).and_then(Value::as_str).filter(|n| !n.is_empty()) {
            eprintln!("\x1b[90m  reman: {n}\x1b[0m");
        }
    }
}

/// "Last time here": asked once when a shell arrives in a folder (fish, via reman-hook).
#[allow(dead_code)] // only reman-hook calls it
pub fn welcome(cwd: Option<String>, session: Option<String>) -> Result<()> {
    let cwd = cwd.or_else(|| std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned()));
    let session = session.or_else(|| std::env::var("REMAN_SESSION").ok()).unwrap_or_default();
    if let Ok(mut c) = Client::connect_timeout(Duration::from_millis(150)) {
        c.set_timeout(Some(Duration::from_millis(300)));
        if let Ok(reply) = c.call(&json!({"op": "welcome", "cwd": cwd, "session": session})) {
            print_note(&reply);
        }
    }
    Ok(())
}

/// On Enter, before a command runs: when the history here says it will fail, one line - the
/// warning, the command that worked instead, its last error, what the fix changes (tab-separated;
/// any but the first may be empty). Nothing otherwise, and nothing when the daemon doesn't answer at once.
#[allow(dead_code)] // only reman-hook calls it
pub fn precheck(cwd: Option<String>, cmd: String) -> Result<()> {
    let cwd = cwd.or_else(|| std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned()));
    if let Ok(mut c) = Client::connect_timeout(Duration::from_millis(60)) {
        c.set_timeout(Some(Duration::from_millis(150)));
        if let Ok(r) = c.call(&json!({"op": "precheck", "command": cmd, "cwd": cwd})) {
            if let Some(w) = r.get("warn").and_then(Value::as_str) {
                let field = |k: &str| r.get(k).and_then(Value::as_str).unwrap_or("").replace(['\t', '\n'], " ");
                println!("{w}\t{}\t{}\t{}", field("fix"), field("error"), field("diff"));
            }
        }
    }
    Ok(())
}

/// Alt+N (fish, Clink): what you'd run next here, idea number `index`: one line - the command,
/// why, its index and how many ideas there are (tab-separated). Nothing with nothing to offer.
#[allow(dead_code)] // only reman-hook calls it
pub fn nextup(cwd: Option<String>, session: Option<String>, index: i64) -> Result<()> {
    let cwd = cwd.or_else(|| std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned()));
    let session = session.or_else(|| std::env::var("REMAN_SESSION").ok()).unwrap_or_default();
    if let Ok(mut c) = Client::connect_timeout(Duration::from_millis(100)) {
        c.set_timeout(Some(Duration::from_millis(400)));
        if let Ok(r) = c.call(&json!({"op": "nextup", "cwd": cwd, "session": session, "index": index})) {
            if let Some(cmd) = r.get("command").and_then(Value::as_str) {
                let reason = r.get("reason").and_then(Value::as_str).unwrap_or("");
                println!("{}\t{}\t{}\t{}", cmd.replace(['\t', '\n'], " "), reason.replace(['\t', '\n'], " "), r["index"], r["of"]);
            }
        }
    }
    Ok(())
}

pub fn print_suggestion(sg: &Value) {
    let Some(cmd) = sg.get("command").and_then(Value::as_str) else { return };
    let lead = match sg.get("kind").and_then(Value::as_str) {
        Some("proven") => "last time this failed you ran",
        Some("same_error") => "the same error was fixed by",
        _ => "did you mean",
    };
    // what it changes, when it's a variant of what failed (adds --build, gti -> git). The key
    // that inserts it is the shell's to name (Clink adds it to this line)
    let how = sg.get("diff").and_then(Value::as_str).map(|d| format!("   ({d})")).unwrap_or_default();
    eprintln!("\x1b[90m  reman: {lead} \u{2192} \x1b[36m{cmd}\x1b[90m{how}\x1b[0m");
}

/// Claude Code PostToolUse / PostToolUseFailure hook: payload JSON on stdin.
pub fn hook_claude() -> Result<()> {
    hook_agent("claude-code")
}

/// One shell command an agent ran, as its hook told it.
#[derive(Default)]
struct AgentRun {
    cmd: String,
    cwd: Option<String>,
    session: String,
    /// None: the tool gave no exit code (the output may still say how it went)
    exit: Option<i64>,
    /// what it printed, to read verdicts from; never sent anywhere
    output: String,
    /// pairs the run with its start (`agent_start`), for a duration
    id: String,
    duration_ms: Option<i64>,
}

fn stdin_json() -> Option<Value> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf).ok()?;
    serde_json::from_str::<Value>(&buf).ok()
}

fn str_at<'a>(v: &'a Value, path: &str) -> Option<&'a str> {
    v.pointer(path).and_then(Value::as_str).filter(|s| !s.trim().is_empty())
}

/// Any value as text: a string as is, anything else as JSON (hooks hand output over both ways).
fn text_of(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(v) => v.to_string(),
    }
}

/// "a command started": its id, so the finished run gets a duration. Fire and forget.
fn mark_start(id: &str) {
    if !id.is_empty() {
        if let Ok(mut c) = Client::connect_timeout(Duration::from_millis(150)) {
            let _ = c.send(&json!({"op": "agent_start", "id": id}));
        }
    }
}

/// Record one agent run: what its output says about each command of the line goes with it (see
/// verdict.rs); the output itself stays here.
/// Record an agent's run.
fn send_run(agent: &str, r: AgentRun) {
    send_run_with(agent, r, false);
}

/// Record an agent's run; with `talk_back`, a failure waits for the daemon's reply (what to tell
/// the agent, through its hook).
fn send_run_with(agent: &str, r: AgentRun, talk_back: bool) -> Option<Value> {
    if r.cmd.trim().is_empty() || ignored(&r.cmd) {
        return None;
    }
    let failed = r.exit.is_some_and(|e| e > 0);
    let cwd = r.cwd.or_else(|| std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned()));
    let mut req = json!({"op": "ingest", "command": r.cmd.trim(), "exit": r.exit, "cwd": cwd, "session": r.session, "actor": format!("agent:{agent}")});
    if !r.id.is_empty() {
        req["start_id"] = json!(r.id);
    }
    if let Some(d) = r.duration_ms {
        req["duration_ms"] = json!(d);
    }
    let reads = crate::verdict::read(&r.cmd, &r.output);
    if reads.iter().any(Option::is_some) {
        req["reads"] = json!(reads);
    }
    // what it printed, when it failed: for "the same error, a different command"
    if failed && !r.output.trim().is_empty() {
        req["error"] = json!(r.output);
    }
    // and the end of it, kept with the run (`reman output`, the finder's ^O)
    if !r.output.trim().is_empty() {
        req["printed"] = json!(tail(&r.output, 32 * 1024));
    }
    push(req, talk_back && failed)
}

/// The end of a long text: at most `n` bytes, cut at a character boundary.
fn tail(s: &str, n: usize) -> &str {
    let mut i = s.len().saturating_sub(n);
    while !s.is_char_boundary(i) {
        i += 1;
    }
    &s[i..]
}

/// What reman tells an agent after a failure, through its hook: that it is retrying a command
/// that keeps failing the same way, that the command is flaky here, or what broke it. None for
/// nothing to say.
fn agent_context(reply: &Value) -> Option<String> {
    let lines: Vec<String> = ["loop", "note"]
        .iter()
        .filter_map(|k| reply.get(*k).and_then(Value::as_str))
        .map(|n| format!("reman: {}", n.trim_end_matches("   (reman why)")))
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// An exit code a tool wrote into its result text: `Exit Code: 1` (Gemini), `Command exited with
/// code 2` (pi, Copilot).
fn exit_in(text: &str) -> Option<i64> {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| regex::Regex::new(r"(?im)\bexit(?:ed)?(?: with)?[ _]code:?\s*(-?\d+)").unwrap());
    re.captures_iter(text).last().and_then(|c| c[1].parse().ok())
}

/// Claude Code's and Codex's hooks (the same format): PreToolUse marks when a command starts,
/// PostToolUse and PostToolUseFailure record it. Claude reports failures; Codex hands hooks only
/// the output, with no exit code, so its runs are recorded with an unknown outcome, never a guess
/// (the output's own words may still tell, see verdict.rs).
pub fn hook_agent(agent: &str) -> Result<()> {
    let Some(data) = stdin_json() else { return Ok(()) };
    // shell commands: Claude's Bash (and on Windows PowerShell) tool, Codex's Bash / shell tool;
    // never VS Code's terminal tool, which reads this same file (its own hook records it)
    if !matches!(data.get("tool_name").and_then(Value::as_str), Some("Bash" | "PowerShell" | "shell" | "local_shell")) {
        return Ok(());
    }
    let cmd = match data.pointer("/tool_input/command") {
        Some(Value::String(s)) => s.trim().to_string(),
        // older Codex: ["bash", "-lc", "the command"]
        Some(Value::Array(a)) => {
            let parts: Vec<&str> = a.iter().filter_map(Value::as_str).collect();
            match parts.as_slice() {
                [_, flag, script] if flag.starts_with('-') && flag.contains('c') => script.trim().to_string(),
                _ => parts.join(" "),
            }
        }
        _ => String::new(),
    };
    if cmd.is_empty() {
        return Ok(());
    }
    let event = data.get("hook_event_name").and_then(Value::as_str).unwrap_or("");
    let id = data.get("tool_use_id").and_then(Value::as_str).unwrap_or("").to_string();
    if event == "PreToolUse" {
        mark_start(&id);
        return Ok(());
    }
    let session = str_at(&data, "/session_id").map(str::to_string).or_else(|| std::env::var("CLAUDE_CODE_SESSION_ID").ok()).unwrap_or_default();
    let resp = data.get("tool_response").cloned().unwrap_or(Value::Null);
    let code = ["exit_code", "exitCode", "returncode", "code"].iter().find_map(|k| resp.get(*k).and_then(Value::as_i64));
    let flag = |k: &str| resp.get(k).and_then(Value::as_bool).unwrap_or(false);
    let exit: Option<i64> = if event == "PostToolUseFailure" || flag("is_error") || flag("interrupted") {
        Some(code.filter(|c| *c != 0).unwrap_or(1))
    } else if code.is_some() {
        code
    } else if agent == "codex" {
        None // Codex gives no exit code: outcome unknown
    } else {
        Some(0) // Claude: a failure would have fired PostToolUseFailure
    };
    let output = [data.get("error"), resp.get("stdout"), resp.get("stderr"), resp.as_str().map(|_| &resp)]
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    let cwd = str_at(&data, "/cwd").map(str::to_string);
    // Claude Code takes what a hook prints as JSON into Claude's context: a failure it keeps
    // retrying, or one reman knows more about, is told to it right there
    let talk = agent == "claude-code";
    let reply = send_run_with(agent, AgentRun { cmd, cwd, session, exit, output, id, duration_ms: None }, talk);
    if let Some(ctx) = reply.as_ref().and_then(agent_context).filter(|_| talk) {
        let ev = if event.is_empty() { "PostToolUse" } else { event };
        println!("{}", json!({"hookSpecificOutput": {"hookEventName": ev, "additionalContext": ctx}}));
    }
    Ok(())
}

/// Cursor's `afterShellExecution` hook: the command, its whole output and its duration; an exit
/// code only in newer versions. No folder in the event itself: the workspace it runs in.
pub fn hook_cursor() -> Result<()> {
    let Some(d) = stdin_json() else { return Ok(()) };
    if d.get("hook_event_name").and_then(Value::as_str).is_some_and(|e| e != "afterShellExecution") {
        return Ok(());
    }
    let cwd = str_at(&d, "/cwd")
        .or_else(|| str_at(&d, "/workspace_roots/0"))
        .map(str::to_string)
        .or_else(|| std::env::var("CURSOR_PROJECT_DIR").ok());
    let output = text_of(d.get("output"));
    send_run(
        "cursor",
        AgentRun {
            cmd: str_at(&d, "/command").unwrap_or("").to_string(),
            cwd,
            session: str_at(&d, "/conversation_id").unwrap_or("").to_string(),
            exit: d.get("exit_code").and_then(Value::as_i64),
            output,
            id: String::new(),
            duration_ms: d.get("duration").and_then(Value::as_i64),
        },
    );
    Ok(())
}

/// Gemini CLI's `AfterTool` hook for `run_shell_command`: its result text says `Exit Code: N`.
pub fn hook_gemini() -> Result<()> {
    let Some(d) = stdin_json() else { return Ok(()) };
    if d.get("tool_name").and_then(Value::as_str) != Some("run_shell_command") {
        return Ok(());
    }
    let base = str_at(&d, "/cwd").map(str::to_string).or_else(|| std::env::var("GEMINI_CWD").ok());
    // `dir_path`: where it ran, absolute or from the workspace root
    let cwd = match (str_at(&d, "/tool_input/dir_path").or_else(|| str_at(&d, "/tool_input/directory")), &base) {
        (Some(p), Some(b)) => Some(std::path::Path::new(b).join(p).to_string_lossy().into_owned()),
        (Some(p), None) => Some(p.to_string()),
        (None, b) => b.clone(),
    };
    let text = [str_at(&d, "/tool_response/llmContent"), str_at(&d, "/tool_response/returnDisplay")].into_iter().flatten().collect::<Vec<_>>().join("\n");
    let error = str_at(&d, "/tool_response/error");
    let exit = exit_in(&text).or(error.map(|_| 1));
    send_run(
        "gemini-cli",
        AgentRun {
            cmd: str_at(&d, "/tool_input/command").unwrap_or("").to_string(),
            cwd,
            session: str_at(&d, "/session_id").unwrap_or("").to_string(),
            exit,
            output: format!("{text}\n{}", error.unwrap_or("")),
            ..Default::default()
        },
    );
    Ok(())
}

/// Windsurf's (Cascade's) `pre_run_command` / `post_run_command` hooks: the command line and its
/// folder, with neither an exit code nor the output, so the outcome stays unknown.
pub fn hook_windsurf() -> Result<()> {
    let Some(d) = stdin_json() else { return Ok(()) };
    let id = str_at(&d, "/execution_id").unwrap_or("").to_string();
    match d.get("agent_action_name").and_then(Value::as_str) {
        Some("pre_run_command") => mark_start(&id),
        Some("post_run_command") => send_run(
            "windsurf",
            AgentRun {
                cmd: str_at(&d, "/tool_info/command_line").unwrap_or("").to_string(),
                cwd: str_at(&d, "/tool_info/cwd").map(str::to_string),
                session: str_at(&d, "/trajectory_id").unwrap_or("").to_string(),
                id,
                ..Default::default()
            },
        ),
        _ => {}
    }
    Ok(())
}

/// GitHub Copilot's hooks (VS Code agent mode, Copilot CLI): PreToolUse / PostToolUse for its
/// terminal tool (VS Code's `run_in_terminal`, the CLI's `bash` / `powershell`). The result is
/// the output text, with an exit code only when it says so: VS Code adds `Command exited with
/// code N` for a failure, the CLI `<exited with exit code N>`. VS Code types the command into a
/// real terminal, so the shell's own record usually has the exact code (daemon: `unpaired`).
pub fn hook_copilot() -> Result<()> {
    let Some(d) = stdin_json() else { return Ok(()) };
    let tool = d.get("tool_name").or_else(|| d.get("toolName")).and_then(Value::as_str).unwrap_or("").to_lowercase();
    let terminal = matches!(tool.as_str(), "bash" | "powershell" | "shell") || (tool.contains("terminal") && (tool.contains("run") || tool.contains("exec")));
    if !terminal {
        return Ok(());
    }
    let id = [str_at(&d, "/tool_use_id"), str_at(&d, "/toolCallId")].into_iter().flatten().next().unwrap_or("").to_string();
    let event = d.get("hook_event_name").or_else(|| d.get("hookEventName")).and_then(Value::as_str).unwrap_or("");
    if event.eq_ignore_ascii_case("PreToolUse") {
        mark_start(&id);
        return Ok(());
    }
    let input = d.get("tool_input").or_else(|| d.get("toolArgs")).cloned().unwrap_or(Value::Null);
    // Copilot CLI hands the arguments over as a JSON string
    let input = match &input {
        Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null),
        v => v.clone(),
    };
    // VS Code: the result text; the Copilot CLI: {resultType, textResultForLlm}
    let result = ["tool_response", "tool_result", "toolResult"].iter().find_map(|k| d.get(*k));
    let output = text_of(result.map(|r| r.get("textResultForLlm").unwrap_or(r)));
    let failed = event.eq_ignore_ascii_case("PostToolUseFailure") || result.and_then(|r| r.get("resultType")).and_then(Value::as_str) == Some("failure");
    send_run(
        "copilot",
        AgentRun {
            cmd: str_at(&input, "/command").unwrap_or("").to_string(),
            cwd: str_at(&d, "/cwd").map(str::to_string),
            session: [str_at(&d, "/session_id"), str_at(&d, "/sessionId")].into_iter().flatten().next().unwrap_or("").to_string(),
            exit: exit_in(&output).or(failed.then_some(1)),
            output,
            id,
            duration_ms: None,
        },
    );
    Ok(())
}

/// A run from reman's own plugins (opencode, pi), already in reman's words:
/// `{"agent", "command", "cwd", "session", "exit", "output", "duration_ms"}`.
pub fn hook_event() -> Result<()> {
    let Some(d) = stdin_json() else { return Ok(()) };
    let agent: String = str_at(&d, "/agent").unwrap_or("agent").chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').take(32).collect();
    send_run(
        &agent,
        AgentRun {
            cmd: str_at(&d, "/command").unwrap_or("").to_string(),
            cwd: str_at(&d, "/cwd").map(str::to_string),
            session: str_at(&d, "/session").unwrap_or("").to_string(),
            exit: d.get("exit").and_then(Value::as_i64),
            output: text_of(d.get("output")),
            id: String::new(),
            duration_ms: d.get("duration_ms").and_then(Value::as_i64),
        },
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_in_result_text() {
        // VS Code's run_in_terminal, on a failure
        assert_eq!(exit_in("npm ERR! code 1\n\nCommand exited with code 2"), Some(2));
        // the Copilot CLI's shell tools
        assert_eq!(exit_in("done\n<exited with exit code 0>"), Some(0));
        // Gemini CLI's run_shell_command
        assert_eq!(exit_in("Command: cargo test\nExit Code: 101"), Some(101));
        assert_eq!(exit_in("Tests: 12 passed"), None);
    }

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
