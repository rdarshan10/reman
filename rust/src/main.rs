//! reman - semantic, provenance-aware shell history for humans and AI agents.
//! One binary: daemon, capture hooks, native finder, MCP server, importers.
mod capture;
mod client;
mod config;
mod connect;
mod daemon;
mod db;
mod describe;
mod dym;
mod embed;
mod fixpairs;
mod flows;
mod http;
mod import;
mod mcp;
mod predict;
mod redact;
mod search;
mod settings;
mod store;
mod tui;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Parser)]
#[command(name = "reman", version, about = "Semantic, provenance-aware shell history (for you and your AI agents)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the warm daemon in the foreground (normally auto-started)
    Daemon {
        #[arg(long)]
        port: Option<u16>,
    },
    /// Search your real commands by meaning / fuzzy text
    Search {
        query: Vec<String>,
        #[arg(short, default_value_t = 10)]
        k: i64,
        /// only commands run in the current folder
        #[arg(long)]
        here: bool,
        #[arg(long)]
        all_variants: bool,
        /// python-parity ranking (semantic only)
        #[arg(long)]
        semantic: bool,
    },
    /// Interactive finder (used by the shell key bindings)
    Find {
        #[arg(long, default_value = "")]
        query: String,
        #[arg(long, default_value = "all")]
        scope: String,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long)]
        result_file: Option<String>,
    },
    /// Did-you-mean for a failed command (proven fixes first)
    Fixes { failed: Vec<String> },
    /// Recurring command sequences
    Flows {
        #[arg(long)]
        here: bool,
    },
    /// Likely next commands here
    Next {
        #[arg(short, default_value_t = 5)]
        k: i64,
    },
    /// Has this command been run, and did it work?
    Check { command: Vec<String> },
    /// Record one executed command (called by shell hooks)
    Record {
        #[arg(long)]
        exit: Option<i64>,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        actor: Option<String>,
        #[arg(long)]
        duration_ms: Option<i64>,
        /// on failure, wait for and print a fix suggestion
        #[arg(long)]
        suggest: bool,
        /// also print the bare fix command on stdout
        #[arg(long)]
        print_fix: bool,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Agent hooks (stdin payload). Currently: claude
    Hook { agent: String },
    /// Import history: atuin | psreadline | bash | zsh | fish
    Import {
        source: String,
        #[arg(long)]
        path: Option<String>,
    },
    /// Print shell integration: powershell | bash | zsh | fish
    Init { shell: String },
    /// Install the binary, import Atuin once, wire your shell profile
    Setup {
        #[arg(long)]
        no_profile: bool,
    },
    /// MCP stdio server for AI agents
    Mcp,
    /// Dump commands, one per line
    Export {
        #[arg(long)]
        here: bool,
        #[arg(long)]
        actor: Option<String>,
        #[arg(long)]
        status: Option<String>,
        #[arg(long, default_value = "")]
        query: String,
    },
    /// Usage statistics
    Stats,
    /// Health report; --fix-dupes removes Atuin/hook double captures
    Doctor {
        #[arg(long)]
        fix_dupes: bool,
        #[arg(long)]
        clean: bool,
        #[arg(long)]
        rebuild_fixpairs: bool,
    },
    /// Forget a command everywhere
    Forget { command: Vec<String> },
    /// Pin / unpin a command
    Pin {
        command: Vec<String>,
        #[arg(long)]
        off: bool,
    },
    /// Latency benchmark against the running daemon
    Bench {
        #[arg(short, default_value_t = 30)]
        n: usize,
    },
    /// Re-embed every command with the current model
    Reindex,
    /// Plug reman into AI agents: `reman connect` (status), `all`, an agent id, or `http`
    Connect {
        /// all | http | claude-code | claude-desktop | codex | cursor | vscode | windsurf | gemini
        targets: Vec<String>,
        /// folder agents may see (repeatable); stored once in ~/.reman/config.json for every agent
        #[arg(long = "root")]
        roots: Vec<String>,
        /// port for `reman connect http` (default 8777)
        #[arg(long)]
        port: Option<u16>,
        /// print paste-able config for any other MCP client
        #[arg(long)]
        print: bool,
    },
    /// Undo `reman connect` for the given agents (or `all`, `http`)
    Disconnect { targets: Vec<String> },
    /// Tool schemas for function calling: mcp | openai | openai-responses | anthropic
    Tools {
        #[arg(long, default_value = "mcp")]
        format: String,
    },
    /// Call one agent tool from the command line: reman call reman_search '{"intent":"..."}'
    Call { tool: String, args: Option<String> },
    /// Ping / stop the daemon
    Ping,
    Stop,
}

fn cwd() -> String {
    std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default()
}

fn results(v: &Value) -> Vec<Value> {
    v.get("results").and_then(Value::as_array).cloned().unwrap_or_default()
}

fn print_rows(rows: &[Value]) {
    for r in rows {
        let status = match r["status"].as_str() {
            Some("ok") => "\x1b[32m+\x1b[0m",
            Some("fail") => "\x1b[31mx\x1b[0m",
            Some("mixed") => "\x1b[33m~\x1b[0m",
            _ => " ",
        };
        println!("\n {status} {}", r["command"].as_str().unwrap_or(""));
        let mut meta = vec![];
        if let Some(s) = r["similarity"].as_f64() {
            meta.push(format!("sim={s:.3}"));
        }
        if let Some(s) = r["fuzzy"].as_f64() {
            meta.push(format!("fuzzy={s:.2}"));
        }
        if let Some(s) = r["typo"].as_f64() {
            meta.push(format!("typo={s:.2} intent={:.2}", r["intent_sim"].as_f64().unwrap_or(0.0)));
        }
        if r["proven"].as_bool() == Some(true) {
            meta.push(format!("PROVEN fix ({})", r["fix_confidence"].as_str().unwrap_or("")));
        }
        if let Some(reason) = r["reason"].as_str() {
            meta.push(reason.to_string());
        }
        meta.push(format!("runs={}", r["run_count"]));
        match r["success_rate"].as_f64() {
            Some(sr) => meta.push(format!("ok={}%", (sr * 100.0).round())),
            None => meta.push("ok=?".into()),
        }
        meta.push(format!("last={}", r["last_run"].as_str().unwrap_or("?")));
        meta.push(r["actor"].as_str().unwrap_or("human").to_string());
        if let Some(v) = r["variants"].as_u64() {
            meta.push(format!("+{} variants", v - 1));
        }
        println!("   \x1b[90m{}\x1b[0m", meta.join("  "));
        if let Some(d) = r["description"].as_str() {
            println!("   \x1b[90m# {d}\x1b[0m");
        }
    }
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("reman: {e:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Daemon { port } => daemon::serve(port.unwrap_or_else(config::port)),
        Cmd::Search { query, k, here, all_variants, semantic } => {
            let mut req = json!({"op": "search", "query": query.join(" "), "k": k, "group": !all_variants});
            if here {
                req["cwd"] = json!(cwd());
            }
            if semantic {
                req["rank"] = json!("semantic");
            }
            let t = Instant::now();
            let r = client::call(&req)?;
            let rows = results(&r);
            print_rows(&rows);
            eprintln!("\n\x1b[90m{} result(s), mode={}, {:.1} ms\x1b[0m", rows.len(), r["mode"].as_str().unwrap_or(""), t.elapsed().as_secs_f64() * 1000.0);
            Ok(())
        }
        Cmd::Find { query, scope, cwd: c, result_file } => {
            let query = if query.is_empty() { std::env::var("REMAN_FIND_QUERY").unwrap_or_default() } else { query };
            tui::run(tui::Opts { query, scope, cwd: c.unwrap_or_else(cwd), result_file })
        }
        Cmd::Fixes { failed } => {
            let r = client::call(&json!({"op": "didyoumean", "query": failed.join(" "), "k": 5, "worked_only": true}))?;
            println!("  command failed: {}\n  did you mean one of these that worked?", failed.join(" "));
            print_rows(&results(&r));
            Ok(())
        }
        Cmd::Flows { here } => {
            let mut req = json!({"op": "flows", "k": 25});
            if here {
                req["cwd"] = json!(cwd());
            }
            let fl = results(&client::call(&req)?);
            if fl.is_empty() {
                println!("reman flows: no recurring sequences yet (need commands run back-to-back, 2+ times)");
            }
            for f in fl {
                let seq: Vec<&str> = f["sequence"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
                println!("  [{}x] {}", f["count"], seq.join("  ->  "));
            }
            Ok(())
        }
        Cmd::Next { k } => {
            let r = client::call(&json!({"op": "next", "cwd": cwd(), "k": k, "session": std::env::var("REMAN_SESSION").unwrap_or_default()}))?;
            if let Some(l) = r["last"].as_str() {
                println!("  last here: {l}");
            }
            print_rows(&results(&r));
            Ok(())
        }
        Cmd::Check { command } => {
            let r = client::call(&json!({"op": "mcp", "tool": "reman_check", "args": {"command": command.join(" ")}, "allow_global": true}))?;
            println!("{}", serde_json::to_string_pretty(&r)?);
            Ok(())
        }
        Cmd::Record { exit, cwd: c, session, actor, duration_ms, suggest, print_fix, command } => {
            capture::record(capture::RecordArgs { command: command.join(" "), exit, cwd: c, session, actor, duration_ms, suggest, print_fix })
        }
        Cmd::Hook { agent } => match agent.as_str() {
            "claude" | "claude-code" => capture::hook_claude(),
            other => bail!("unknown agent hook {other:?} (supported: claude)"),
        },
        Cmd::Import { source, path } => {
            let mut total = 0u64;
            let mut new = 0u64;
            loop {
                let r = client::call(&json!({"op": "import", "source": source, "path": path, "limit": 5000}))?;
                total += r["ingested"].as_u64().unwrap_or(0);
                new += r["new"].as_u64().unwrap_or(0);
                if r["remaining"].as_u64().unwrap_or(0) == 0 || r["ingested"].as_u64() == Some(0) {
                    break;
                }
            }
            println!("reman import {source}: {total} runs ({new} new commands)");
            Ok(())
        }
        Cmd::Init { shell } => {
            print!("{}", init_script(&shell, &std::env::current_exe()?)?);
            Ok(())
        }
        Cmd::Setup { no_profile } => setup(no_profile),
        Cmd::Mcp => mcp::serve_stdio(),
        Cmd::Export { here, actor, status, query } => {
            let mut req = if query.trim().is_empty() { json!({"op": "recent", "k": 0}) } else { json!({"op": "search", "query": query, "k": 200}) };
            if here {
                req["cwd"] = json!(cwd());
            }
            if let Some(a) = actor {
                req["actor"] = json!(a);
            }
            if let Some(s) = status {
                req["status"] = json!(s);
            }
            for r in results(&client::call(&req)?) {
                println!("{}", r["command"].as_str().unwrap_or(""));
            }
            Ok(())
        }
        Cmd::Stats => {
            let s = client::call(&json!({"op": "stats"}))?;
            println!("reman stats");
            println!("  commands {}   runs {}   executions logged {}", s["commands"], s["runs"], s["executions"]);
            println!("  ok runs {}   failed runs {}   you {}   agents {}", s["ok_runs"], s["failed_runs"], s["human_runs"], s["agent_runs"]);
            println!("  proven fix-pairs {}   pinned {}", s["fix_pairs"], s["pinned"]);
            println!("\n  most run:");
            for t in s["top"].as_array().into_iter().flatten() {
                println!("    {:>5}  {}", t["runs"], t["command"].as_str().unwrap_or(""));
            }
            println!("\n  busiest folders:");
            for t in s["folders"].as_array().into_iter().flatten() {
                println!("    {:>5}  {}", t["runs"], t["cwd"].as_str().unwrap_or(""));
            }
            println!("\n  most failing:");
            for t in s["failing"].as_array().into_iter().flatten() {
                println!("    {:>5}  {}", t["fails"], t["command"].as_str().unwrap_or(""));
            }
            Ok(())
        }
        Cmd::Doctor { fix_dupes, clean, rebuild_fixpairs } => doctor(fix_dupes, clean, rebuild_fixpairs),
        Cmd::Forget { command } => {
            let r = client::call(&json!({"op": "forget", "command": command.join(" ")}))?;
            println!("reman forget: removed {} row(s)", r["removed"]);
            Ok(())
        }
        Cmd::Pin { command, off } => {
            let r = client::call(&json!({"op": "pin", "command": command.join(" "), "on": !off}))?;
            println!("{}", if r["ok"].as_bool() == Some(true) { "ok" } else { "unknown command" });
            Ok(())
        }
        Cmd::Bench { n } => bench(n),
        Cmd::Reindex => {
            let r = client::call(&json!({"op": "reindex"}))?;
            println!("reman reindex: {} commands re-embedded", r["reembedded"]);
            Ok(())
        }
        Cmd::Connect { targets, roots, port, print } => connect_cmd(&targets, &roots, port, print),
        Cmd::Disconnect { targets } => disconnect_cmd(&targets),
        Cmd::Tools { format } => {
            println!("{}", serde_json::to_string_pretty(&mcp::tools_as(&format)?)?);
            Ok(())
        }
        Cmd::Call { tool, args } => {
            let args: Value = match args {
                Some(a) => serde_json::from_str(&a).context("args must be a JSON object")?,
                None => json!({}),
            };
            let v = mcp::Bridge::new(mcp::Policy::from_env()).call(&tool, &args)?;
            println!("{}", serde_json::to_string_pretty(&v)?);
            Ok(())
        }
        Cmd::Ping => {
            let t = Instant::now();
            let r = client::call(&json!({"op": "ping"}))?;
            println!("{} ({:.1} ms)", r, t.elapsed().as_secs_f64() * 1000.0);
            Ok(())
        }
        Cmd::Stop => {
            if client::daemon_up() {
                client::Client::connect()?.call(&json!({"op": "shutdown"}))?;
                println!("reman daemon stopped");
            } else {
                println!("reman daemon not running");
            }
            Ok(())
        }
    }
}

fn init_script(shell: &str, exe: &Path) -> Result<String> {
    let tpl = match shell {
        "powershell" | "pwsh" => include_str!("init/reman.ps1"),
        "bash" => include_str!("init/reman.bash"),
        "zsh" => include_str!("init/reman.zsh"),
        "fish" => include_str!("init/reman.fish"),
        other => bail!("unknown shell {other:?} (powershell|bash|zsh|fish)"),
    };
    let fix = |p: &Path| {
        let s = p.to_string_lossy().into_owned();
        if shell == "powershell" || shell == "pwsh" { s } else { s.replace('\\', "/") }
    };
    Ok(tpl
        .replace("__REMAN__", &fix(exe))
        .replace("__HOOK__", &fix(&hook_exe(exe)))
        .replace("__PORT__", &config::port().to_string())
        .replace("__SPOOL__", &fix(&config::spool_path())))
}

/// The slim capture binary next to `exe`, falling back to `exe` itself (`reman record` works too).
fn hook_exe(exe: &Path) -> PathBuf {
    let h = exe.with_file_name(if cfg!(windows) { "reman-hook.exe" } else { "reman-hook" });
    if h.exists() { h } else { exe.to_path_buf() }
}

/// The binaries agents should launch: the installed copy when present (stable path), else this one.
fn agent_exes() -> Result<(PathBuf, PathBuf)> {
    let installed = config::bin_dir().join(if cfg!(windows) { "reman.exe" } else { "reman" });
    let exe = if installed.exists() { installed } else { std::env::current_exe()? };
    let hook = hook_exe(&exe);
    Ok((exe, hook))
}

fn connect_cmd(targets: &[String], roots: &[String], port: Option<u16>, print: bool) -> Result<()> {
    let (exe, hook) = agent_exes()?;
    if print {
        println!("{}", connect::generic_snippet(&exe));
        return Ok(());
    }
    // the boundary: one list of folders for every agent
    let mut st = settings::load();
    if !roots.is_empty() || st.mcp_roots.is_empty() {
        st.mcp_roots = connect::resolve_roots(roots);
        settings::save(&st)?;
    }
    if targets.is_empty() {
        println!("reman connect - agents on this machine\n");
        for (id, name) in connect::AGENTS {
            let state = match (connect::installed(id), connect::status(id, &exe)) {
                (_, Some(true)) => "\x1b[32mconnected\x1b[0m",
                (_, Some(false)) => "\x1b[33mconnected (old path - reconnect)\x1b[0m",
                (true, None) => "installed, not connected",
                (false, None) => "\x1b[90mnot installed\x1b[0m",
            };
            println!("  {id:<15} {name:<30} {state}");
        }
        let http = st.http.as_ref().map(|h| format!("\x1b[32mon\x1b[0m  http://127.0.0.1:{}/mcp", h.port)).unwrap_or_else(|| "off".into());
        println!("  {:<15} {:<30} {http}", "http", "HTTP endpoint (any agent/SDK)");
        println!("\n  agents may see commands from: {}", st.mcp_roots.join("  |  "));
        println!("\n  reman connect all              connect every installed agent");
        println!("  reman connect <id> | http      one agent / the HTTP endpoint (OpenAI Agents SDK, LangChain, curl)");
        println!("  reman connect --root <dir> ... change which folders agents may see (all agents at once)");
        println!("  reman connect --print          config to paste into any other MCP client");
        return Ok(());
    }
    let ids: Vec<String> = if targets.iter().any(|t| t == "all") {
        connect::AGENTS.iter().filter(|(id, _)| connect::installed(id)).map(|(id, _)| id.to_string()).collect()
    } else {
        targets.to_vec()
    };
    println!("agents may see commands from: {}\n", st.mcp_roots.join("  |  "));
    for id in &ids {
        if id == "http" {
            connect_http(&mut st, port)?;
            continue;
        }
        let Some((_, name)) = connect::AGENTS.iter().find(|(a, _)| a == id) else {
            println!("  \x1b[31mx\x1b[0m {id}: unknown agent (known: {}, http)", connect::AGENTS.iter().map(|a| a.0).collect::<Vec<_>>().join(", "));
            continue;
        };
        match connect::connect(id, &exe, &hook) {
            Ok(detail) => println!("  \x1b[32m+\x1b[0m {name:<30} {detail}"),
            Err(e) => println!("  \x1b[31mx\x1b[0m {name:<30} {e:#}"),
        }
    }
    println!("\nrestart the connected apps (or reload their MCP servers) to pick reman up.");
    Ok(())
}

fn connect_http(st: &mut settings::Settings, port: Option<u16>) -> Result<()> {
    let prev = st.http.clone();
    let port = port.or(prev.as_ref().map(|h| h.port)).unwrap_or(8777);
    let token = prev.map(|h| h.token).unwrap_or_else(settings::new_token);
    st.http = Some(settings::Http { port, token });
    settings::save(st)?;
    let r = client::call(&json!({"op": "http_enable"}))?;
    if r["ok"].as_bool() != Some(true) {
        bail!("daemon could not start the endpoint: {}", r["error"].as_str().unwrap_or("?"));
    }
    let url = format!("http://127.0.0.1:{port}");
    println!("  \x1b[32m+\x1b[0m HTTP endpoint                 {url}/mcp   (token in {})", settings::path().display());
    println!(
        "\n  MCP (Streamable HTTP) - OpenAI Agents SDK:\n\
         \x20   from agents.mcp import MCPServerStreamableHttp\n\
         \x20   reman = MCPServerStreamableHttp(params={{\"url\": \"{url}/mcp\", \"headers\": {{\"Authorization\": \"Bearer <token>\"}}}})\n\
         \n  Plain function calling (any SDK):\n\
         \x20   GET  {url}/tools?format=openai        tool schemas (also: anthropic, openai-responses, mcp)\n\
         \x20   POST {url}/tools/reman_search         body = the tool's JSON arguments\n\
         \n  curl -H \"Authorization: Bearer <token>\" {url}/tools/reman_search -d '{{\"intent\":\"run the tests\"}}'"
    );
    Ok(())
}

fn disconnect_cmd(targets: &[String]) -> Result<()> {
    let (exe, _) = agent_exes()?;
    let all = targets.iter().any(|t| t == "all");
    let ids: Vec<String> = if all {
        connect::AGENTS.iter().filter(|(id, _)| connect::status(id, &exe).is_some()).map(|(id, _)| id.to_string()).chain(["http".to_string()]).collect()
    } else {
        targets.to_vec()
    };
    for id in &ids {
        if id == "http" {
            let mut st = settings::load();
            if st.http.take().is_some() {
                settings::save(&st)?;
                println!("  - HTTP endpoint disabled (requests are refused immediately)");
            }
            continue;
        }
        match connect::disconnect(id) {
            Ok(d) => println!("  - {id}: {d}"),
            Err(e) => println!("  x {id}: {e:#}"),
        }
    }
    Ok(())
}

fn bench(n: usize) -> Result<()> {
    let mut c = client::Client::connect()?;
    let here = cwd();
    let queries = ["run database migrations", "tear down containers", "git push", "dock comp up", "install python deps", "list kubernetes pods"];
    let cases: Vec<(&str, Box<dyn Fn(usize) -> Value>)> = vec![
        ("ping", Box::new(|_| json!({"op": "ping"}))),
        ("search", Box::new(move |i| json!({"op": "search", "query": queries[i % queries.len()], "k": 40}))),
        ("search (fresh)", Box::new(move |i| json!({"op": "search", "query": format!("{} {i}", queries[i % queries.len()]), "k": 40}))),
        ("search folder", Box::new({ let h = here.clone(); move |i| json!({"op": "search", "query": queries[i % queries.len()], "k": 40, "cwd": h}) })),
        ("recent 200", Box::new(|_| json!({"op": "recent", "k": 200}))),
        ("didyoumean", Box::new(|i| {
            let typo = ["dcoker ps", "gti status", "pyhton app.py"][i % 3];
            json!({"op": "didyoumean", "query": typo, "k": 8})
        })),
        ("flows", Box::new(|_| json!({"op": "flows", "k": 40}))),
        ("next", Box::new({ let h = here.clone(); move |_| json!({"op": "next", "cwd": h, "k": 5}) })),
        ("mcp search", Box::new(|i| json!({"op": "mcp", "tool": "reman_search", "args": {"intent": queries[i % queries.len()]}, "allow_global": true}))),
    ];
    println!("{:<16} {:>9} {:>9} {:>9}", "op", "p50 ms", "p95 ms", "max ms");
    for (name, mk) in cases {
        let mut t: Vec<f64> = (0..n)
            .map(|i| {
                let s = Instant::now();
                let _ = c.call(&mk(i));
                s.elapsed().as_secs_f64() * 1000.0
            })
            .collect();
        t.sort_by(|a, b| a.total_cmp(b));
        let p = |q: f64| t[((t.len() as f64 - 1.0) * q).round() as usize];
        println!("{name:<16} {:>9.2} {:>9.2} {:>9.2}", p(0.5), p(0.95), t[t.len() - 1]);
    }
    let s = Instant::now();
    let _ = std::process::Command::new(std::env::current_exe()?).args(["record", "--exit", "0", "--", "reman bench probe"]).status();
    println!("{:<16} {:>9.2}   (process spawn + push)", "record", s.elapsed().as_secs_f64() * 1000.0);
    c.call(&json!({"op": "forget", "command": "reman bench probe"}))?;
    Ok(())
}

fn doctor(fix_dupes: bool, clean: bool, rebuild_fixpairs: bool) -> Result<()> {
    println!("reman doctor ({})", config::VERSION);
    let dbp = config::db_path();
    println!("  db          : {} ({:.1} MB)", dbp.display(), std::fs::metadata(&dbp).map(|m| m.len() as f64 / 1e6).unwrap_or(0.0));
    let spool = config::spool_path();
    let pending = std::fs::read_to_string(&spool).map(|s| s.lines().count()).unwrap_or(0);
    println!("  spool       : {pending} pending run(s)");
    println!("  atuin db    : {}", config::atuin_db_path().map(|p| p.display().to_string()).unwrap_or_else(|| "not found (fine - reman captures itself)".into()));
    match client::call(&json!({"op": "ping"})) {
        Ok(p) => println!("  daemon      : up, engine={} pid={} indexed={}", p["engine"].as_str().unwrap_or("python"), p["pid"], p["indexed"]),
        Err(e) => println!("  daemon      : DOWN ({e})"),
    }
    for p in profile_paths() {
        let txt = std::fs::read_to_string(&p).unwrap_or_default();
        let state = if txt.contains(">>> reman >>>") { "wired (rust)" } else if txt.contains("reman_init") { "LEGACY python wiring - run `reman setup`" } else { "not wired" };
        println!("  profile     : {} -> {state}{}", p.display(), if txt.contains("atuin init") { " (atuin still hooked)" } else { "" });
    }
    if fix_dupes {
        let r = client::call(&json!({"op": "fix_dupes"}))?;
        println!("  fix-dupes   : removed {} duplicate execution(s)", r["removed"]);
    }
    if clean {
        let r = client::call(&json!({"op": "clean"}))?;
        println!("  clean       : removed {} junk command(s), recomputed pass/fail", r["removed"]);
    }
    if rebuild_fixpairs {
        let r = client::call(&json!({"op": "fixpairs_rebuild"}))?;
        println!("  fix-pairs   : {} pair(s) found in history", r["pairs"]);
    }
    Ok(())
}

fn profile_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if cfg!(windows) {
        for exe in ["powershell", "pwsh"] {
            if let Ok(o) = std::process::Command::new(exe).args(["-NoProfile", "-Command", "$PROFILE"]).output() {
                let p = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if !p.is_empty() {
                    out.push(PathBuf::from(p));
                }
            }
        }
    }
    out
}

/// Remove blank-line-separated blocks that wire Atuin or the legacy Python reman, then append
/// one managed block. Everything else in the profile is kept byte-for-byte.
fn rewire_profile(text: &str, exe: &Path) -> String {
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let norm = text.replace("\r\n", "\n");
    let mut kept: Vec<&str> = Vec::new();
    let mut in_managed = false;
    let mut blocks: Vec<Vec<&str>> = vec![vec![]];
    for ln in norm.lines() {
        if ln.contains(">>> reman >>>") {
            in_managed = true;
            continue;
        }
        if ln.contains("<<< reman <<<") {
            in_managed = false;
            continue;
        }
        if in_managed {
            continue;
        }
        if ln.trim().is_empty() {
            blocks.push(vec![]);
        } else {
            blocks.last_mut().unwrap().push(ln);
        }
    }
    for b in blocks.iter().filter(|b| !b.is_empty()) {
        let joined = b.join("\n");
        if joined.contains("atuin init") || joined.contains("reman_init") || joined.contains("$_remanPy") {
            continue;
        }
        if !kept.is_empty() {
            kept.push("");
        }
        kept.extend(b.iter());
    }
    let exe = exe.to_string_lossy();
    let managed = [
        "# >>> reman >>>".to_string(),
        "# Reman shell integration (managed by `reman setup`): captures every command with cwd, exit".to_string(),
        "# code, duration and session (replaces Atuin) and binds UpArrow / Ctrl+R / Alt+F.".to_string(),
        format!("if (Test-Path \"{exe}\") {{ & \"{exe}\" init powershell | Out-String | Invoke-Expression }}"),
        "# <<< reman <<<".to_string(),
    ];
    let mut out: Vec<String> = kept.iter().map(|s| s.to_string()).collect();
    if !out.is_empty() {
        out.push(String::new());
    }
    out.extend(managed);
    out.join(nl) + nl
}

fn setup(no_profile: bool) -> Result<()> {
    println!("reman setup\n{}", "=".repeat(46));
    // 1. install a stable copy (a running daemon locks its exe on Windows; builds must not fight it)
    let me = std::env::current_exe()?;
    let bin = config::bin_dir();
    std::fs::create_dir_all(&bin)?;
    let installed = bin.join(if cfg!(windows) { "reman.exe" } else { "reman" });
    if me.canonicalize().ok() != installed.canonicalize().ok() {
        if client::daemon_up() {
            if let Ok(mut c) = client::Client::connect_timeout(std::time::Duration::from_millis(300)) {
                let _ = c.call(&json!({"op": "shutdown"}));
            }
            std::thread::sleep(std::time::Duration::from_millis(800));
        }
        if std::fs::copy(&me, &installed).is_err() {
            // still locked by some other reman process (a daemon on another port, a finder):
            // Windows lets a running exe be renamed, so move it aside and install fresh
            let aside = installed.with_extension(format!("old-{}.exe", config::now()));
            std::fs::rename(&installed, &aside).with_context(|| format!("{} is locked", installed.display()))?;
            std::fs::copy(&me, &installed).with_context(|| format!("installing to {}", installed.display()))?;
        }
        // the slim capture binary ships next to the main one
        let hook_src = me.with_file_name(if cfg!(windows) { "reman-hook.exe" } else { "reman-hook" });
        let hook_dst = bin.join(hook_src.file_name().unwrap_or_default());
        if hook_src.exists() && std::fs::copy(&hook_src, &hook_dst).is_err() {
            let aside = hook_dst.with_extension(format!("old-{}.exe", config::now()));
            let _ = std::fs::rename(&hook_dst, &aside);
            std::fs::copy(&hook_src, &hook_dst).with_context(|| format!("installing {}", hook_dst.display()))?;
        }
        for old in std::fs::read_dir(&bin)?.flatten() {
            if old.file_name().to_string_lossy().contains(".old-") {
                let _ = std::fs::remove_file(old.path()); // fails harmlessly while still running
            }
        }
    }
    println!("  binary          : {}", installed.display());
    // 2. warm daemon, started from the installed copy (in-process client: no captured child
    //    pipes for the daemon to inherit)
    let mut c = client::Client::connect_with(&installed)?;
    let p = c.call(&json!({"op": "ping"}))?;
    println!("  daemon          : up (engine={}, pid={}, {} commands)", p["engine"].as_str().unwrap_or("?"), p["pid"], p["indexed"]);
    // 3. one-time Atuin import (final catch-up), then scrub the double captures it causes
    if config::atuin_db_path().is_some() {
        let (mut runs, mut new) = (0, 0);
        loop {
            let r = c.call(&json!({"op": "import", "source": "atuin", "limit": 5000}))?;
            runs += r["ingested"].as_u64().unwrap_or(0);
            new += r["new"].as_u64().unwrap_or(0);
            if r["remaining"].as_u64().unwrap_or(0) == 0 || r["ingested"].as_u64() == Some(0) {
                break;
            }
        }
        println!("  atuin import    : {runs} runs ({new} new commands)");
    }
    let r = c.call(&json!({"op": "fix_dupes"}))?;
    println!("  double captures : removed {}", r["removed"]);
    let r = c.call(&json!({"op": "fixpairs_rebuild"}))?;
    println!("  fix-pairs       : {} found in history", r["pairs"]);
    // 4. shell profile
    if !no_profile && cfg!(windows) {
        for p in profile_paths() {
            let raw = std::fs::read(&p).unwrap_or_default();
            if raw.iter().take(400).any(|b| *b == 0) {
                println!("  profile         : {} looks UTF-16 - add `& \"{}\" init powershell | Out-String | Invoke-Expression` manually", p.display(), installed.display());
                continue;
            }
            let text = String::from_utf8_lossy(raw.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&raw)).into_owned();
            let new = rewire_profile(&text, &installed);
            if new == text {
                println!("  profile         : already wired ({})", p.display());
                continue;
            }
            if p.exists() {
                std::fs::copy(&p, p.with_extension("ps1.reman-bak"))?;
            } else if let Some(d) = p.parent() {
                std::fs::create_dir_all(d)?;
            }
            std::fs::write(&p, new)?;
            println!("  profile         : WIRED {} (backup .reman-bak; atuin hook removed) -> open a new shell", p.display());
        }
    }
    if !cfg!(windows) {
        let shell = std::env::var("SHELL").unwrap_or_default();
        let kind = ["zsh", "fish"].into_iter().find(|s| shell.contains(s)).unwrap_or("bash");
        println!("  shell ({kind})    : add to your rc file:  eval \"$({} init {kind})\"", installed.display());
    }
    println!("  claude code     : hook   -> \"{}\" claude", hook_exe(&installed).display());
    println!("                    mcp    -> claude mcp add reman -- \"{}\" mcp   (env REMAN_MCP_ROOT=<project dirs>)", installed.display());
    println!("\n  done.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewires_profile_idempotently() {
        let old = "# Atuin shell-history capture\nif (Get-Command atuin) {\n    atuin init powershell | Out-String | Invoke-Expression\n}\n\n# mine\nSet-Alias ll ls\n\n# Reman legacy\n$_remanPy = \"x\"; $_remanInit = \"y\"\nif (1) { & $_remanPy $_remanInit powershell }\n";
        let exe = Path::new(r"C:\Users\u\.reman\bin\reman.exe");
        let new = rewire_profile(old, exe);
        assert!(!new.contains("atuin init"));
        assert!(!new.contains("_remanPy"));
        assert!(new.contains("Set-Alias ll ls"));
        assert!(new.contains(">>> reman >>>"));
        assert_eq!(rewire_profile(&new, exe), new);
    }

    #[test]
    fn init_templates_substitute() {
        for sh in ["powershell", "bash", "zsh", "fish"] {
            let s = init_script(sh, Path::new("/x/reman")).unwrap();
            assert!(!s.contains("__REMAN__") && !s.contains("__PORT__") && !s.contains("__SPOOL__"), "{sh}");
        }
    }
}
