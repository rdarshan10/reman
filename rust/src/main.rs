//! reman - semantic, provenance-aware shell history for humans and AI agents.
//! One binary: daemon, capture hooks, native finder, MCP server, importers.
mod capture;
mod ai;
mod client;
mod complete;
mod config;
mod connect;
mod daemon;
mod db;
mod describe;
mod dym;
mod embed;
mod errors;
mod fixpairs;
mod flows;
mod http;
mod import;
mod insight;
mod mcp;
mod predict;
mod redact;
mod search;
mod settings;
mod store;
mod settings_ui;
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
        /// type the pick onto the shell's next prompt instead of printing it (Command Prompt's `r`)
        #[arg(long)]
        to_prompt: bool,
        /// starting query, e.g. `reman find docker`
        words: Vec<String>,
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
    /// What you did in this folder last time
    Here,
    /// Remove reman: shells, coding tools, PATH, the program; your history too with --purge
    Uninstall {
        /// also delete your history, settings and the search model (~/.reman)
        #[arg(long, conflicts_with = "keep_data")]
        purge: bool,
        /// keep your history, settings and the search model (the default when not asked)
        #[arg(long)]
        keep_data: bool,
        /// don't ask
        #[arg(long, short)]
        yes: bool,
        /// only list what would be removed
        #[arg(long)]
        dry_run: bool,
    },
    /// Mask secrets in history saved before reman masked them at capture (a dry run without --apply)
    Scrub {
        #[arg(long)]
        apply: bool,
    },
    /// A command that used to work here fails now: what ran here since it last worked
    /// (default: the last command that failed here)
    Why { command: Vec<String> },
    /// How this project is run: the commands that worked here, by task, and the usual sequences
    Runbook {
        /// print JSON (what agents get from the reman_runbook tool)
        #[arg(long)]
        json: bool,
        /// never ask a language model: the runbook from your history alone
        #[arg(long = "static")]
        no_ai: bool,
        /// ask the model again, even if it already wrote this project's runbook
        #[arg(long)]
        refresh: bool,
    },
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
        #[arg(last = true)]
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
    /// Print shell integration: powershell | bash | zsh | fish | cmd (via Clink)
    Init { shell: String },
    /// One-step onboarding: install, start the daemon, wire your shells, connect your coding tools
    Setup {
        /// leave shell profiles / rc files alone
        #[arg(long)]
        no_profile: bool,
        /// don't connect coding tools (Claude Code, Codex, Cursor, VS Code, ...)
        #[arg(long)]
        no_connect: bool,
    },
    /// Settings page: coding tools, shared folders, privacy, shells
    Settings,
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
        /// REPLACE the folders agents may see (repeatable); stored once in ~/.reman/config.json
        #[arg(long = "root")]
        roots: Vec<String>,
        /// add a folder agents may see (repeatable), keeping the others
        #[arg(long = "add-root")]
        add_roots: Vec<String>,
        /// stop sharing a folder with agents (repeatable)
        #[arg(long = "remove-root")]
        remove_roots: Vec<String>,
        /// port for `reman connect http` (default 8777)
        #[arg(long)]
        port: Option<u16>,
        /// print paste-able config for any other MCP client
        #[arg(long)]
        print: bool,
        /// share generic commands from old, folder-less history with agents: on | off
        #[arg(long = "old-history", value_parser = ["on", "off"])]
        old_history: Option<String>,
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
    /// Tab-completion backend for the shell integrations (prints value<TAB>help lines)
    #[command(hide = true)]
    Complete {
        /// the partial word under the cursor (`--cur=` when empty)
        #[arg(long, default_value = "", allow_hyphen_values = true)]
        cur: String,
        /// the raw text before the partial word, starting with `reman` (split here, not by the shell)
        #[arg(long, allow_hyphen_values = true)]
        line: Option<String>,
        /// or: the finished words after `reman`
        #[arg(last = true)]
        words: Vec<String>,
    },
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
        Cmd::Find { query, scope, cwd: c, result_file, to_prompt, words } => {
            let query = if !query.is_empty() {
                query
            } else if !words.is_empty() {
                words.join(" ")
            } else {
                std::env::var("REMAN_FIND_QUERY").unwrap_or_default()
            };
            tui::run(tui::Opts { query, scope, cwd: c.unwrap_or_else(cwd), result_file, to_prompt })
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
        Cmd::Uninstall { purge, keep_data, yes, dry_run } => uninstall(if purge { Some(true) } else if keep_data { Some(false) } else { None }, yes, dry_run),
        Cmd::Scrub { apply } => {
            let r = client::call(&json!({"op": "scrub", "apply": apply}))?;
            let (mask, drop) = (r["mask"].as_u64().unwrap_or(0), r["drop"].as_u64().unwrap_or(0));
            if mask + drop == 0 {
                println!("No secrets found in your history.");
                return Ok(());
            }
            let verb = if apply { "" } else { "would be " };
            println!("{mask} command(s) {verb}kept with the secret masked, {drop} {verb}forgotten (a private key block: nothing worth keeping around it).");
            for e in r["examples"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                println!("  {e}");
            }
            if !apply {
                println!("\nRun `reman scrub --apply` to do it. New commands are already masked as they're recorded.");
            }
            Ok(())
        }
        Cmd::Here => {
            let r = client::call(&json!({"op": "here", "cwd": cwd()}))?;
            if r["found"] != json!(true) {
                println!("reman has nothing you did in this folder yet.");
                return Ok(());
            }
            println!("Last time here ({}):", r["ago"].as_str().unwrap_or("?"));
            for (n, s) in r["steps"].as_array().into_iter().flatten().filter_map(Value::as_str).enumerate() {
                println!("  {}. {s}", n + 1);
            }
            Ok(())
        }
        Cmd::Why { command } => {
            let mut req = json!({"op": "why", "cwd": cwd()});
            if !command.is_empty() {
                req["command"] = json!(command.join(" "));
            }
            let r = client::call(&req)?;
            if r["found"] != json!(true) {
                if let Some(c) = r["command"].as_str() {
                    println!("`{c}`");
                }
                println!("{}", r["reason"].as_str().unwrap_or("Nothing to explain."));
                return Ok(());
            }
            let cmd = r["command"].as_str().unwrap_or("");
            println!("`{cmd}` worked here {} times; last {}, then it failed {}.", r["worked"], r["last_ok"].as_str().unwrap_or("?"), r["first_fail"].as_str().unwrap_or("?"));
            let between: Vec<&str> = r["between"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            if between.is_empty() {
                println!("Nothing else ran in this folder in between: look outside it (a service, the network, files changed by another tool).");
            } else {
                println!("In between, in this folder (changes to dependencies, branch, schema and files first):");
                for b in between {
                    println!("  · {b}");
                }
            }
            println!("\nSince its last success:");
            for t in r["timeline"].as_array().into_iter().flatten() {
                let mark = match t["exit"].as_i64() {
                    Some(0) => "\x1b[32m✓\x1b[0m",
                    Some(_) => "\x1b[31m✗\x1b[0m",
                    None => "·",
                };
                let c = t["command"].as_str().unwrap_or("").lines().next().unwrap_or("");
                let bold = if t["target"] == json!(true) { "\x1b[1m" } else { "" };
                println!("  {mark} {bold}{c}\x1b[0m   \x1b[90m{}\x1b[0m", t["ago"].as_str().unwrap_or(""));
            }
            Ok(())
        }
        Cmd::Runbook { json: as_json, no_ai, refresh } => {
            let mut r = client::call(&json!({"op": "runbook", "cwd": cwd(), "fresh": no_ai || refresh}))?;
            let has_unplaced = r["unplaced"].as_array().is_some_and(|a| !a.is_empty());
            // a language model, if one is available, arranges and explains (never invents)
            if !no_ai && r.get("model").is_none() && (r["found"] == json!(true) || has_unplaced) {
                if let Some(m) = ai::find() {
                    eprintln!("\x1b[90mreman: asking {} to arrange the runbook (--static skips this)...\x1b[0m", m.label());
                    match ai::runbook(&m, &r) {
                        Ok(x) => {
                            let _ = ai::save(&r, &x, &m.model);
                            r = ai::merge(&r, &x, &m.model);
                        }
                        Err(e) => eprintln!("\x1b[90mreman: the model didn't answer usefully ({e:#}); here is the runbook from your history alone.\x1b[0m"),
                    }
                }
            }
            let r = ai::without_unplaced(r);
            if as_json {
                println!("{}", serde_json::to_string_pretty(&r)?);
                return Ok(());
            }
            if r["found"] != json!(true) {
                println!("reman doesn't know how this project is run yet: run it a few times first.");
                return Ok(());
            }
            println!("# How this project is run\n");
            if let Some(s) = r["summary"].as_str().filter(|s| !s.is_empty()) {
                println!("{s}\n");
            }
            let first: Vec<&str> = r["getting_started"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            if !first.is_empty() {
                println!("## Getting started\n");
                for (n, c) in first.iter().enumerate() {
                    println!("{}. `{c}`", n + 1);
                }
                println!();
            }
            for sec in r["sections"].as_array().into_iter().flatten() {
                println!("## {}\n", sec["title"].as_str().unwrap_or(""));
                for c in sec["commands"].as_array().into_iter().flatten() {
                    let rec = if c["failed"].as_u64() == Some(0) { format!("worked {}x", c["worked"]) } else { format!("worked {} of {}", c["worked"], c["runs"]) };
                    let note = c["note"].as_str().map(|n| format!(": {n}")).unwrap_or_default();
                    println!("- `{}`{note} · {rec} · last {}", c["command"].as_str().unwrap_or("").replace('\n', " "), c["last_run"].as_str().unwrap_or("?"));
                }
                println!();
            }
            let flows: Vec<&Value> = r["flows"].as_array().into_iter().flatten().collect();
            if !flows.is_empty() {
                println!("## Usual sequences\n");
                for f in flows {
                    let steps: Vec<String> = f["steps"].as_array().into_iter().flatten().filter_map(Value::as_str).map(|s| format!("`{}`", s.replace('\n', " "))).collect();
                    println!("- {} ({}x)", steps.join(" → "), f["count"]);
                }
                println!();
            }
            match r["model"].as_str() {
                Some(m) => println!("_Arranged by {m} from commands that worked here. Every command is one that really ran; reman never generates them._"),
                None => println!("_From reman: commands that worked here, most used first._"),
            }
            Ok(())
        }
        Cmd::Check { command } => {
            let r = client::call(&json!({"op": "mcp", "tool": "reman_check", "args": {"command": command.join(" ")}, "allow_global": true}))?;
            println!("{}", serde_json::to_string_pretty(&r)?);
            Ok(())
        }
        Cmd::Record { exit, cwd: c, session, actor, duration_ms, suggest, print_fix, command } => {
            // cmd.exe (Clink) passes the command in the environment: its quoting can't carry it intact
            let command = if command.is_empty() { std::env::var("REMAN_RECORD_CMD").unwrap_or_default() } else { command.join(" ") };
            if command.trim().is_empty() {
                bail!("nothing to record: pass the command after `--`");
            }
            capture::record(capture::RecordArgs { command, exit, cwd: c, session, actor, duration_ms, suggest, print_fix })
        }
        Cmd::Hook { agent } => match agent.as_str() {
            "claude" | "claude-code" => capture::hook_claude(),
            "codex" => capture::hook_agent("codex"),
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
        Cmd::Setup { no_profile, no_connect } => setup(no_profile, no_connect),
        Cmd::Settings => settings_ui::run(),
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
        Cmd::Connect { targets, roots, add_roots, remove_roots, port, print, old_history } => {
            connect_cmd(&targets, &roots, &add_roots, &remove_roots, port, print, old_history.as_deref())
        }
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
        Cmd::Complete { cur, line, words } => {
            use clap::CommandFactory;
            // Clink (cmd.exe) hands the line over in the environment, like `record`
            let line = line.or_else(|| std::env::var("REMAN_COMPLETE_LINE").ok());
            let cur = if cur.is_empty() { std::env::var("REMAN_COMPLETE_CUR").unwrap_or_default() } else { cur };
            let words = line.map(|l| complete::split_line(&l)).unwrap_or(words);
            complete::print(&Cli::command(), &words, &cur);
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
        "cmd" | "clink" => include_str!("init/reman.lua"),
        other => bail!("unknown shell {other:?} (powershell|bash|zsh|fish|cmd)"),
    };
    let fix = |p: &Path| {
        let s = p.to_string_lossy().into_owned();
        if matches!(shell, "powershell" | "pwsh" | "cmd" | "clink") { s } else { s.replace('\\', "/") }
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

/// Folders with history agents can't see (see complete::unshared_folders). Starts the daemon if
/// needed - `reman connect` is interactive, unlike tab completion.
fn unshared_folders(roots: &[String]) -> Vec<(String, u64)> {
    let _ = client::call(&json!({"op": "ping"}));
    complete::unshared_folders(roots)
}

fn connect_cmd(targets: &[String], roots: &[String], add: &[String], remove: &[String], port: Option<u16>, print: bool, old_history: Option<&str>) -> Result<()> {
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
    if !add.is_empty() || !remove.is_empty() {
        for r in connect::resolve_roots(add) {
            if !st.mcp_roots.iter().any(|x| config::norm_path(x) == config::norm_path(&r)) {
                println!("  + agents may now see {r}");
                st.mcp_roots.push(r);
            }
        }
        for r in connect::resolve_roots(remove) {
            let before = st.mcp_roots.len();
            st.mcp_roots.retain(|x| config::norm_path(x) != config::norm_path(&r));
            if st.mcp_roots.len() < before {
                println!("  - agents no longer see {r}");
            } else {
                println!("  ({r} wasn't shared)");
            }
        }
        settings::save(&st)?;
    }
    if let Some(v) = old_history {
        st.share_old_history = v == "on";
        settings::save(&st)?;
        println!("old history (no recorded folder): {}", if st.share_old_history { "generic commands shared with agents" } else { "hidden from agents" });
    }
    let old_line = if st.share_old_history { "plus generic commands from old, folder-less history" } else { "old, folder-less history hidden (--old-history on to share generic commands)" };
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
        println!("\n  agents may see commands from: {}", connect::roots_line(&st.mcp_roots));
        println!("                                {old_line}");
        let hidden = unshared_folders(&st.mcp_roots);
        if !hidden.is_empty() {
            println!("\n  folders with history that agents can't see (busiest first):");
            for (f, runs) in hidden.iter().take(6) {
                println!("    {f:<50} {runs:>5} runs");
            }
            println!("    share one:  reman connect --add-root \"{}\"", hidden[0].0);
        }
        println!("\n  reman connect all              connect every installed agent");
        println!("  reman connect <id> | http      one agent / the HTTP endpoint (OpenAI Agents SDK, LangChain, curl)");
        println!("  reman connect --add-root <dir> let agents see another folder (--remove-root to undo)");
        println!("  reman connect --root <dir> ... replace the whole list (all agents at once)");
        println!("  reman connect --old-history on share generic commands (no paths/quotes/hosts) from folder-less old history");
        println!("  reman connect --print          config to paste into any other MCP client");
        return Ok(());
    }
    let ids: Vec<String> = if targets.iter().any(|t| t == "all") {
        connect::AGENTS.iter().filter(|(id, _)| connect::installed(id)).map(|(id, _)| id.to_string()).collect()
    } else {
        targets.to_vec()
    };
    println!("agents may see commands from: {}\n                              {old_line}\n", connect::roots_line(&st.mcp_roots));
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
    let hidden = unshared_folders(&st.mcp_roots);
    if !hidden.is_empty() {
        let names: Vec<&str> = hidden.iter().take(3).map(|h| h.0.as_str()).collect();
        println!("\n{} other folder(s) with history stay private from agents ({}{}).", hidden.len(), names.join(", "), if hidden.len() > 3 { ", ..." } else { "" });
        println!("share one with: reman connect --add-root <folder>   (`reman connect` lists them)");
    }
    println!("\nrestart the connected apps (or reload their MCP servers) to pick reman up.");
    Ok(())
}

/// Turn the local HTTP endpoint on (keeps an existing port/token). Returns the port.
fn enable_http(st: &mut settings::Settings, port: Option<u16>) -> Result<u16> {
    let prev = st.http.clone();
    let port = port.or(prev.as_ref().map(|h| h.port)).unwrap_or(8777);
    let token = prev.map(|h| h.token).unwrap_or_else(settings::new_token);
    st.http = Some(settings::Http { port, token });
    settings::save(st)?;
    let r = client::call(&json!({"op": "http_enable"}))?;
    if r["ok"].as_bool() != Some(true) {
        bail!("daemon could not start the endpoint: {}", r["error"].as_str().unwrap_or("?"));
    }
    Ok(port)
}

/// Turn it off: requests are refused immediately. Returns whether it was on.
fn disable_http(st: &mut settings::Settings) -> Result<bool> {
    let was = st.http.take().is_some();
    if was {
        settings::save(st)?;
    }
    Ok(was)
}

fn connect_http(st: &mut settings::Settings, port: Option<u16>) -> Result<()> {
    let port = enable_http(st, port)?;
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
            if disable_http(&mut settings::load())? {
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
    for (exe, policy, locked) in ps_policy_blocks() {
        let fix = if locked { "set by group policy: ask your administrator" } else { "run: Set-ExecutionPolicy RemoteSigned -Scope CurrentUser" };
        println!("  scripts     : {} BLOCKS profile scripts ({policy}), so reman never loads there; {fix}", ps_name(exe));
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
    // tests: which profile files to use instead of the real ones
    if let Some(list) = std::env::var_os("REMAN_PS_PROFILES") {
        return std::env::split_paths(&list).collect();
    }
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

/// Clink's profile folder (it loads every .lua there), as Clink itself reports it (`clink info`,
/// "state"): its `~` means %LOCALAPPDATA%, not the home folder, so don't guess. None when Clink
/// isn't hooked into cmd.exe.
fn clink_profile_dir() -> Option<PathBuf> {
    let out = std::process::Command::new("reg").args(["query", r"HKCU\Software\Microsoft\Command Processor", "/v", "AutoRun"]).output().ok()?;
    let autorun = String::from_utf8_lossy(&out.stdout).to_string();
    if !autorun.to_lowercase().contains("clink") {
        return None;
    }
    // the AutoRun value starts with the quoted path of clink.bat
    let bat = autorun.split('"').nth(1).filter(|p| p.to_lowercase().ends_with("clink.bat"))?.to_string();
    let info = std::process::Command::new("cmd").args(["/d", "/c", "call", &bat, "info"]).output().ok()?;
    String::from_utf8_lossy(&info.stdout)
        .lines()
        .find_map(|l| l.trim().strip_prefix("state").map(|r| r.trim_start().trim_start_matches(':').trim().to_string()))
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::data_local_dir().map(|d| d.join("clink")))
}

/// Command Prompt without add-ons: DOSKEY macros `r` / `rr` open the finder and type the pick
/// onto the next prompt. cmd loads them at start through its AutoRun setting; whatever AutoRun
/// already ran keeps running (ours is appended, once).
fn wire_cmd_macros(exe: &Path) -> Result<PathBuf> {
    let file = config::home().join("cmd-macros.txt");
    std::fs::create_dir_all(config::home())?;
    let e = exe.display();
    std::fs::write(
        &file,
        format!("r=\"{e}\" find --scope folder --to-prompt $*\r\nrr=\"{e}\" find --scope all --to-prompt $*\r\n"),
    )?;
    let key = r"HKCU\Software\Microsoft\Command Processor";
    let ours = format!("doskey /macrofile=\"{}\"", file.display());
    let cur = std::process::Command::new("reg").args(["query", key, "/v", "AutoRun"]).output().ok();
    let cur = cur
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .and_then(|s| s.lines().find_map(|l| l.split_once("REG_SZ").or_else(|| l.split_once("REG_EXPAND_SZ")).map(|(_, v)| v.trim().to_string())))
        .unwrap_or_default();
    // keep the other AutoRun commands, minus any whose program is gone (an uninstalled Clink
    // leaves `"...\clink.bat" inject` behind, and cmd would print an error at every start)
    let mut parts: Vec<String> = cur
        .split(" & ")
        .map(str::trim)
        .filter(|p| !p.is_empty() && !p.contains("cmd-macros.txt"))
        .filter(|p| match p.strip_prefix('"').and_then(|r| r.split_once('"')) {
            Some((prog, _)) if prog.contains('\\') => Path::new(prog).exists(),
            _ => true,
        })
        .map(String::from)
        .collect();
    if cur.contains("cmd-macros.txt") && parts.join(" & ") == cur.split(" & ").map(str::trim).filter(|p| !p.contains("cmd-macros.txt")).collect::<Vec<_>>().join(" & ") {
        return Ok(file);
    }
    parts.push(ours);
    let value = parts.join(" & ");
    let st = std::process::Command::new("reg").args(["add", key, "/v", "AutoRun", "/t", "REG_SZ", "/d", &value, "/f"]).output()?;
    if !st.status.success() {
        bail!("reg add failed: {}", String::from_utf8_lossy(&st.stderr).trim());
    }
    Ok(file)
}

/// The file in Clink's profile folder: it runs `reman init cmd` at each cmd start, so every new
/// Command Prompt gets the integration of the reman that's installed now.
fn clink_loader(exe: &Path) -> String {
    format!(
        "-- reman (managed by `reman setup`): loads reman's Command Prompt integration.\n\
         -- Delete this file to turn reman off in cmd.exe.\n\
         local exe = [[{}]]\n\
         local h = io.popen('\"\"' .. exe .. '\" init cmd 2>nul\"')\n\
         if h then\n    local src = h:read(\"*a\")\n    h:close()\n    local f = load(src, \"reman-init\")\n    if f then f() end\nend\n",
        exe.display()
    )
}

// ---------------------------------------------------------------------------------------------
// uninstall: what setup and the installers did, undone, each found by reman's own markers
// ---------------------------------------------------------------------------------------------

/// A PowerShell profile without reman's managed block (and the blank line before it); None
/// when it has none. Everything else stays as it was.
fn unwire_profile(text: &str) -> Option<String> {
    if !text.contains(">>> reman >>>") {
        return None;
    }
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out: Vec<&str> = Vec::new();
    let mut inside = false;
    for ln in text.lines() {
        if ln.contains(">>> reman >>>") {
            inside = true;
            while out.last().is_some_and(|l| l.trim().is_empty()) {
                out.pop();
            }
            continue;
        }
        if inside {
            inside = !ln.contains("<<< reman <<<");
            continue;
        }
        out.push(ln);
    }
    Some(if out.is_empty() { String::new() } else { out.join(nl) + nl })
}

/// A zsh / bash / fish rc file without the line `reman setup` added (and its blank line).
fn unwire_rc(text: &str) -> Option<String> {
    const MARK: &str = "# reman shell integration";
    if !text.contains(MARK) {
        return None;
    }
    let mut out: Vec<&str> = Vec::new();
    for ln in text.lines() {
        if ln.contains(MARK) {
            while out.last().is_some_and(|l| l.trim().is_empty()) {
                out.pop();
            }
            continue;
        }
        out.push(ln);
    }
    Some(if out.is_empty() { String::new() } else { out.join("\n") + "\n" })
}

/// cmd.exe's AutoRun without reman's macros: None = nothing of ours there; Some(None) = remove
/// the value (only ours was there); Some(Some(v)) = keep the rest.
fn autorun_without_macros(cur: &str) -> Option<Option<String>> {
    if !cur.contains("cmd-macros.txt") {
        return None;
    }
    let rest: Vec<&str> = cur.split(" & ").map(str::trim).filter(|p| !p.is_empty() && !p.contains("cmd-macros.txt")).collect();
    Some((!rest.is_empty()).then(|| rest.join(" & ")))
}

fn user_path() -> String {
    std::process::Command::new("powershell")
        .args(["-NoProfile", "-Command", "[Environment]::GetEnvironmentVariable('Path','User')"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn same_dir(a: &str, b: &Path) -> bool {
    config::norm_path(a.trim_end_matches(['\\', '/'])) == config::norm_path(b.to_string_lossy().trim_end_matches(['\\', '/']))
}

fn dir_size(p: &Path) -> u64 {
    std::fs::read_dir(p)
        .map(|rd| {
            rd.flatten()
                .map(|e| match e.metadata() {
                    Ok(m) if m.is_dir() => dir_size(&e.path()),
                    Ok(m) => m.len(),
                    Err(_) => 0,
                })
                .sum()
        })
        .unwrap_or(0)
}

/// Delete a folder. The running reman can't delete its own file on Windows: then a hidden,
/// detached PowerShell does it once this process has exited (retrying while agents that were
/// still running `reman mcp` let go of it).
fn remove_dir(dir: &Path) -> Result<bool> {
    let me = std::env::current_exe().ok().and_then(|p| p.canonicalize().ok());
    let inside = me.is_some_and(|m| dir.canonicalize().is_ok_and(|d| m.starts_with(d)));
    if !(cfg!(windows) && inside) {
        if dir.exists() {
            std::fs::remove_dir_all(dir)?;
        }
        return Ok(false);
    }
    let script = "Start-Sleep 2; for ($i = 0; $i -lt 60 -and (Test-Path -LiteralPath $env:REMAN_RM); $i++) { Remove-Item -LiteralPath $env:REMAN_RM -Recurse -Force -ErrorAction SilentlyContinue; Start-Sleep 1 }";
    let mut cmd = std::process::Command::new("powershell");
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", script]).env("REMAN_RM", dir).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // a hidden console of its own (a fully detached PowerShell, with no console, may just
        // exit), its own process group, and out of the caller's job when that's allowed
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB);
        if cmd.spawn().is_ok() {
            return Ok(true);
        }
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }
    cmd.spawn()?;
    Ok(true)
}

fn ask(question: &str) -> bool {
    use std::io::Write;
    print!("{question} ");
    let _ = std::io::stdout().flush();
    let mut a = String::new();
    let _ = std::io::stdin().read_line(&mut a);
    a.trim().to_lowercase().starts_with('y')
}

/// `reman uninstall`: remove reman from the coding tools, the shells, Command Prompt and PATH,
/// stop the daemon and delete the program; the history, settings and search model too with
/// --purge. Lists everything first and asks. The PowerShell script policy is left alone: other
/// tools may need it.
fn uninstall(purge: Option<bool>, yes: bool, dry: bool) -> Result<()> {
    use std::io::IsTerminal;
    let home = config::home();
    let bin = config::bin_dir();
    let exe = bin.join(if cfg!(windows) { "reman.exe" } else { "reman" });
    let sandbox = connect::sandbox();
    let real = sandbox.is_none();
    let user_home = sandbox.clone().or_else(dirs::home_dir).unwrap_or_default();

    // --- what's there ----------------------------------------------------------------------
    let mut plan: Vec<String> = connect::remove_everywhere(&exe, true)?.into_iter().map(|a| format!("coding tool     {a}")).collect();
    let mut st = settings::load();
    if st.http.is_some() {
        plan.push("coding tools    the local HTTP endpoint".into());
    }
    let profiles: Vec<PathBuf> = profile_paths().into_iter().filter(|p| std::fs::read_to_string(p).is_ok_and(|t| t.contains(">>> reman >>>"))).collect();
    plan.extend(profiles.iter().map(|p| format!("PowerShell      reman's block in {}", p.display())));
    let rcs: Vec<PathBuf> = [user_home.join(".bashrc"), user_home.join(".zshrc"), user_home.join(".config").join("fish").join("config.fish")]
        .into_iter()
        .filter(|p| std::fs::read_to_string(p).is_ok_and(|t| t.contains("# reman shell integration")))
        .collect();
    plan.extend(rcs.iter().map(|p| format!("shell           reman's line in {}", p.display())));
    let autorun = if cfg!(windows) && real { autorun_without_macros(&cmd_autorun()) } else { None };
    if autorun.is_some() {
        plan.push("Command Prompt  the r / rr macros (its AutoRun entry)".into());
    }
    let clink = if cfg!(windows) && real { clink_profile_dir().map(|d| d.join("reman.lua")).filter(|f| f.exists()) } else { None };
    if let Some(f) = &clink {
        plan.push(format!("Command Prompt  {}", f.display()));
    }
    let path_now = if cfg!(windows) && real { user_path() } else { String::new() };
    let path_new: Vec<&str> = path_now.split(';').filter(|p| !p.is_empty() && !same_dir(p, &bin)).collect();
    let path_changes = cfg!(windows) && real && path_new.len() != path_now.split(';').filter(|p| !p.is_empty()).count();
    if path_changes {
        plan.push(format!("PATH            {}", bin.display()));
    }
    plan.push("the daemon      stopped".into());
    plan.push(format!("the program     {}", bin.display()));
    let data_mb = (dir_size(&home).saturating_sub(dir_size(&bin))) as f64 / 1e6;

    println!("{}", if dry { "reman uninstall would remove:" } else { "reman uninstall removes:" });
    for p in &plan {
        println!("  {p}");
    }
    println!("\nYour history, settings and the search model ({}, {data_mb:.0} MB) stay unless you pass --purge.", home.display());
    if dry {
        return Ok(());
    }
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if !yes {
        if !interactive {
            bail!("nothing removed: run `reman uninstall --yes` to confirm (or --dry-run to only list)");
        }
        if !ask("\nRemove all of this? [y/N]") {
            println!("Nothing removed.");
            return Ok(());
        }
    }
    let purge = purge.unwrap_or_else(|| interactive && !yes && ask(&format!("Also delete your history, settings and the search model ({data_mb:.0} MB)? [y/N]")));

    // --- remove -----------------------------------------------------------------------------
    for line in connect::remove_everywhere(&exe, false)? {
        println!("  removed  {line}");
    }
    if disable_http(&mut st)? {
        settings::save(&st)?;
        println!("  removed  the local HTTP endpoint");
    }
    for p in &profiles {
        let raw = std::fs::read(p).unwrap_or_default();
        let text = String::from_utf8_lossy(raw.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&raw)).into_owned();
        if let Some(new) = unwire_profile(&text) {
            std::fs::write(p, new)?;
            println!("  removed  reman's block from {}", p.display());
        }
    }
    for p in &rcs {
        if let Some(new) = std::fs::read_to_string(p).ok().and_then(|t| unwire_rc(&t)) {
            std::fs::write(p, new)?;
            println!("  removed  reman's line from {}", p.display());
        }
    }
    if let Some(rest) = autorun {
        let key = r"HKCU\Software\Microsoft\Command Processor";
        let o = match &rest {
            Some(v) => std::process::Command::new("reg").args(["add", key, "/v", "AutoRun", "/t", "REG_SZ", "/d", v, "/f"]).output()?,
            None => std::process::Command::new("reg").args(["delete", key, "/v", "AutoRun", "/f"]).output()?,
        };
        if o.status.success() {
            println!("  removed  the Command Prompt macros (AutoRun{})", if rest.is_some() { ", keeping the rest of it" } else { "" });
        }
        let _ = std::fs::remove_file(home.join("cmd-macros.txt"));
    }
    if let Some(f) = &clink {
        std::fs::remove_file(f)?;
        println!("  removed  {}", f.display());
    }
    if path_changes {
        let o = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", "[Environment]::SetEnvironmentVariable('Path', $env:REMAN_NEW_PATH, 'User')"])
            .env("REMAN_NEW_PATH", path_new.join(";"))
            .output()?;
        if o.status.success() {
            println!("  removed  {} from your PATH", bin.display());
        }
    }
    // the daemon holds the database and its own copy of the program
    if client::call(&json!({"op": "shutdown"})).is_ok() {
        println!("  stopped  the daemon");
        std::thread::sleep(std::time::Duration::from_millis(800));
    }
    let target = if purge { &home } else { &bin };
    let later = remove_dir(target)?;
    println!("  {}  {}", if later { "removing" } else { "removed " }, target.display());
    println!("\nreman is uninstalled{}. Open a new terminal.", if later { " (its folder goes in a few seconds, once this command exits)" } else { "" });
    if !purge {
        println!("Your history is still in {}: reinstall to pick up where you left off, or delete the folder.", home.display());
    }
    println!("PowerShell's script policy was left as it is (other tools may rely on it).");
    Ok(())
}

/// A title for plain output: the logo (orange cursor, xterm 202) with `what` beside its last row
/// on a terminal; one plain line when the output is a file or pipe, or NO_COLOR is set.
fn print_title(what: &str) {
    use std::io::IsTerminal;
    if std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none() {
        let rows = tui::logo_rows();
        for (i, (letters, cursor)) in rows.iter().enumerate() {
            let side = if i == rows.len() - 1 { format!("   \x1b[1m{what}\x1b[0m") } else { String::new() };
            println!("  {letters}\x1b[38;5;202m{cursor}\x1b[0m{side}");
        }
        println!();
    } else {
        println!("reman {what}\n{}", "=".repeat(46));
    }
}

fn setup(no_profile: bool, no_connect: bool) -> Result<()> {
    print_title("setup");
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
        for line in wire_ps_profiles(&installed)? {
            println!("  profile         : {line}");
        }
        // a wired profile does nothing if PowerShell won't run it
        check_ps_policy();
    }
    // 5. Command Prompt, through Clink (cmd.exe itself has no per-command hook or key bindings)
    if !no_profile && cfg!(windows) {
        match clink_profile_dir() {
            Some(dir) => {
                let f = dir.join("reman.lua");
                std::fs::create_dir_all(&dir)?;
                std::fs::write(&f, clink_loader(&installed))?;
                println!("  cmd (clink)     : WIRED {} -> open a new Command Prompt", f.display());
            }
            None => match wire_cmd_macros(&installed) {
                Ok(f) => println!("  cmd             : WIRED `r` (this folder) / `rr` (everywhere) via {} -> open a new Command Prompt", f.display()),
                Err(e) => println!("  cmd             : could not set up the `r` macro: {e:#}"),
            },
        }
    }
    // 5b. zsh / bash / fish: one marked line in the rc file of the login shell
    if !no_profile && !cfg!(windows) {
        match wire_unix_rc(&installed) {
            Ok((kind, rc, true)) => println!("  shell ({kind})    : WIRED {} -> open a new terminal", rc.display()),
            Ok((kind, rc, false)) => println!("  shell ({kind})    : already wired ({})", rc.display()),
            Err(e) => println!("  shell           : {e:#}"),
        }
    }
    // 6. coding tools: plug into every one that's installed (backups kept; `reman disconnect all` undoes)
    if !no_connect && std::env::var_os("REMAN_NO_CONNECT").is_none() {
        let hook = hook_exe(&installed);
        let found: Vec<(&str, &str)> = connect::AGENTS.iter().copied().filter(|(id, _)| connect::installed(id)).collect();
        if found.is_empty() {
            println!("  coding tools    : none found (connect one later: reman settings)");
        }
        for (id, name) in found {
            match connect::connect(id, &installed, &hook) {
                Ok(_) => println!("  coding tools    : connected {name}"),
                Err(e) => println!("  coding tools    : {name} failed: {e:#}"),
            }
        }
        println!("                    agents see {}", connect::roots_line(&settings::load().mcp_roots));
    }
    println!("\n  done. Change any of this later with: reman settings");
    Ok(())
}

/// PowerShell: put reman's managed block in every profile (backup `.reman-bak`). One status line
/// per profile.
fn wire_ps_profiles(installed: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for p in profile_paths() {
        let raw = std::fs::read(&p).unwrap_or_default();
        if raw.iter().take(400).any(|b| *b == 0) {
            out.push(format!("{} looks UTF-16 - add `& \"{}\" init powershell | Out-String | Invoke-Expression` manually", p.display(), installed.display()));
            continue;
        }
        let text = String::from_utf8_lossy(raw.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(&raw)).into_owned();
        let new = rewire_profile(&text, installed);
        if new == text {
            out.push(format!("already wired ({})", p.display()));
            continue;
        }
        if p.exists() {
            std::fs::copy(&p, p.with_extension("ps1.reman-bak"))?;
        } else if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        std::fs::write(&p, new)?;
        out.push(format!("WIRED {} (backup .reman-bak) -> open a new shell", p.display()));
    }
    Ok(out)
}

/// PowerShell runs no profile script under the "Restricted" or "AllSigned" execution policy, and
/// Restricted is the default for Windows PowerShell on Windows 10/11. Then reman's profile line
/// never runs and Up stays plain history. Per installed shell that blocks: (exe, policy, set by
/// group policy so the user can't change it). Asked the way a NEW window sees it: without the
/// Process scope this process may have inherited (PSExecutionPolicyPreference).
fn ps_policy_blocks() -> Vec<(&'static str, String, bool)> {
    let mut out = Vec::new();
    if !cfg!(windows) {
        return out;
    }
    let probe = "$g = ((Get-ExecutionPolicy -Scope MachinePolicy) -ne 'Undefined') -or ((Get-ExecutionPolicy -Scope UserPolicy) -ne 'Undefined'); \"$(Get-ExecutionPolicy)|$g\"";
    for exe in ["powershell", "pwsh"] {
        let Ok(o) = std::process::Command::new(exe).args(["-NoProfile", "-Command", probe]).env_remove("PSExecutionPolicyPreference").output() else { continue };
        let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
        let Some((policy, locked)) = s.split_once('|') else { continue };
        if matches!(policy, "Restricted" | "AllSigned") {
            out.push((exe, policy.to_string(), locked.eq_ignore_ascii_case("true")));
        }
    }
    out
}

fn ps_name(exe: &str) -> &'static str {
    if exe == "pwsh" { "PowerShell 7" } else { "Windows PowerShell" }
}

/// Let this user's own scripts (the profile) run: RemoteSigned for the current user only, the
/// setting Microsoft recommends for this and that Scoop and oh-my-posh also need.
fn allow_ps_scripts(exe: &str) -> Result<()> {
    let o = std::process::Command::new(exe)
        .args(["-NoProfile", "-Command", "Set-ExecutionPolicy RemoteSigned -Scope CurrentUser -Force"])
        .env_remove("PSExecutionPolicyPreference")
        .output()?;
    if !o.status.success() {
        bail!("{}", String::from_utf8_lossy(&o.stderr).trim());
    }
    Ok(())
}

/// Setup: PowerShell profiles are wired, but will they run? If a shell blocks scripts, ask (in a
/// terminal, default yes) to allow them for this user; otherwise say exactly what to run.
fn check_ps_policy() {
    use std::io::{IsTerminal, Write};
    for (exe, policy, locked) in ps_policy_blocks() {
        let name = ps_name(exe);
        if locked {
            println!("  script policy   : {name} blocks profile scripts ({policy}), set by group policy: reman can't load in it. Ask your administrator.");
            continue;
        }
        let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
        let yes = if std::env::var_os("REMAN_ALLOW_SCRIPTS").is_some() {
            true
        } else if interactive {
            print!("\n  {name} is set to block profile scripts ({policy}), so reman can't start in new {name} windows.\n  Allow scripts you create for your user only (Set-ExecutionPolicy RemoteSigned -Scope CurrentUser)? [Y/n] ");
            let _ = std::io::stdout().flush();
            let mut a = String::new();
            let _ = std::io::stdin().read_line(&mut a);
            !a.trim().to_lowercase().starts_with('n')
        } else {
            false
        };
        if yes {
            match allow_ps_scripts(exe) {
                Ok(()) => println!("  script policy   : {name} now runs your own scripts (RemoteSigned, your user only) -> reman loads in new windows"),
                Err(e) => println!("  script policy   : could not change it ({e}). Run: Set-ExecutionPolicy RemoteSigned -Scope CurrentUser"),
            }
        } else {
            println!("  script policy   : {name} blocks profile scripts ({policy}); reman loads there once you run:");
            println!("                    Set-ExecutionPolicy RemoteSigned -Scope CurrentUser");
        }
    }
}

/// Is PowerShell wired (a profile holds reman's managed block)?
fn ps_wired() -> Option<PathBuf> {
    profile_paths().into_iter().find(|p| std::fs::read_to_string(p).is_ok_and(|t| t.contains(">>> reman >>>")))
}

/// cmd.exe's AutoRun value ("" when unset).
fn cmd_autorun() -> String {
    std::process::Command::new("reg")
        .args(["query", r"HKCU\Software\Microsoft\Command Processor", "/v", "AutoRun"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .and_then(|s| s.lines().find_map(|l| l.split_once("REG_SZ").or_else(|| l.split_once("REG_EXPAND_SZ")).map(|(_, v)| v.trim().to_string())))
        .unwrap_or_default()
}

/// Is the login shell's rc file wired (zsh / bash / fish)?
fn unix_rc_wired() -> Option<(&'static str, PathBuf)> {
    let home = dirs::home_dir()?;
    let shell = std::env::var("SHELL").unwrap_or_default();
    let (kind, rc) = if shell.ends_with("zsh") {
        ("zsh", home.join(".zshrc"))
    } else if shell.ends_with("fish") {
        ("fish", home.join(".config").join("fish").join("config.fish"))
    } else {
        ("bash", home.join(".bashrc"))
    };
    std::fs::read_to_string(&rc).ok().filter(|t| t.contains("# reman shell integration")).map(|_| (kind, rc))
}

/// zsh / bash / fish: add `eval "$(reman init <shell>)"` (fish: `| source`) to the login shell's rc
/// file, once. Returns (shell, rc file, whether it was added now).
fn wire_unix_rc(exe: &Path) -> Result<(&'static str, PathBuf, bool)> {
    let home = dirs::home_dir().context("no home folder")?;
    let shell = std::env::var("SHELL").unwrap_or_default();
    let (kind, rc, line) = if shell.ends_with("zsh") {
        ("zsh", home.join(".zshrc"), format!("eval \"$(\"{}\" init zsh)\"", exe.display()))
    } else if shell.ends_with("fish") {
        ("fish", home.join(".config").join("fish").join("config.fish"), format!("\"{}\" init fish | source", exe.display()))
    } else {
        ("bash", home.join(".bashrc"), format!("eval \"$(\"{}\" init bash)\"", exe.display()))
    };
    const MARK: &str = "# reman shell integration";
    let text = std::fs::read_to_string(&rc).unwrap_or_default();
    if text.contains(MARK) {
        return Ok((kind, rc, false));
    }
    if let Some(d) = rc.parent() {
        std::fs::create_dir_all(d)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&rc)?;
    use std::io::Write as _;
    writeln!(f, "\n{line}  {MARK}")?;
    Ok((kind, rc, true))
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
    fn uninstall_leaves_everything_else() {
        let exe = Path::new(r"C:\Users\u\.reman\bin\reman.exe");
        let mine = "Set-Alias ll ls\r\nfunction hi { 'hi' }\r\n";
        let wired = rewire_profile(mine, exe);
        assert!(wired.contains(">>> reman >>>"));
        let back = unwire_profile(&wired).unwrap();
        assert!(back.contains("Set-Alias ll ls") && back.contains("function hi") && !back.contains("reman"), "{back:?}");
        assert_eq!(unwire_profile(mine), None);

        let rc = "export EDITOR=vim\nalias gs='git status'\n\neval \"$(\"/home/u/.reman/bin/reman\" init zsh)\"  # reman shell integration\n";
        assert_eq!(unwire_rc(rc).unwrap(), "export EDITOR=vim\nalias gs='git status'\n");
        assert_eq!(unwire_rc("export A=1\n"), None);

        let macros = r#"doskey /macrofile="C:\Users\u\.reman\cmd-macros.txt""#;
        assert_eq!(autorun_without_macros(macros), Some(None));
        assert_eq!(autorun_without_macros(&format!(r#""C:\clink\clink.bat" inject & {macros}"#)), Some(Some(r#""C:\clink\clink.bat" inject"#.to_string())));
        assert_eq!(autorun_without_macros(r#""C:\clink\clink.bat" inject"#), None);
    }

    #[test]
    fn init_templates_substitute() {
        for sh in ["powershell", "bash", "zsh", "fish"] {
            let s = init_script(sh, Path::new("/x/reman")).unwrap();
            assert!(!s.contains("__REMAN__") && !s.contains("__PORT__") && !s.contains("__SPOOL__"), "{sh}");
        }
    }
}
