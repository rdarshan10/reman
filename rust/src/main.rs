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
mod git;
mod flows;
mod http;
mod import;
mod insight;
mod mcp;
mod notify;
mod output;
mod pty;
mod keys;
mod predict;
mod redact;
mod search;
mod settings;
mod store;
mod timewords;
mod settings_ui;
mod tui;
mod unwrap;
mod verdict;

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
        /// how many results
        #[arg(short, long = "limit", default_value_t = 10)]
        k: i64,
        /// only commands run in the current folder
        #[arg(long)]
        here: bool,
        /// only commands run in this folder
        #[arg(long)]
        cwd: Option<String>,
        /// only commands that only ever failed
        #[arg(long, conflicts_with = "worked")]
        failed: bool,
        /// only commands that worked
        #[arg(long)]
        worked: bool,
        /// only what you ran (`you`), or what agents ran (`agents`)
        #[arg(long, value_parser = by_values())]
        by: Option<String>,
        /// only commands run after this: yesterday, "last week", monday, 2026-09-28
        #[arg(long)]
        after: Option<String>,
        /// only commands run before this
        #[arg(long)]
        before: Option<String>,
        /// print each result with this template: {command} {folder} {runs} {status} {last} {by}
        #[arg(long)]
        format: Option<String>,
        /// print the results as JSON
        #[arg(long)]
        json: bool,
        #[arg(long)]
        all_variants: bool,
        /// python-parity ranking (semantic only)
        #[arg(long)]
        semantic: bool,
    },
    /// Delete commands from your history: those containing the text (or matching --regex),
    /// narrowed by filters. Shows what it would delete and asks first
    Delete {
        contains: Vec<String>,
        /// match a regex instead of plain text
        #[arg(long)]
        regex: Option<String>,
        /// only commands that only ever failed
        #[arg(long)]
        failed: bool,
        /// only what you ran (`you`), or what agents ran (`agents`)
        #[arg(long, value_parser = by_values())]
        by: Option<String>,
        /// only commands last run before this: "last month", 2026-09-01
        #[arg(long)]
        before: Option<String>,
        /// only in this folder
        #[arg(long)]
        cwd: Option<String>,
        /// don't ask
        #[arg(long)]
        yes: bool,
    },
    /// Apply your ignore_commands / ignore_folders rules (config.json) to history saved before
    /// you added them. Shows what it would remove and asks first
    Prune {
        #[arg(long)]
        yes: bool,
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
    /// The folder where you ran what the words describe (or whose path has them): prints it, for
    /// `rcd` to go there. A number last picks another match (`goto alembic 2`); no words lists
    /// where you were lately
    Goto { words: Vec<String> },
    /// What you ran yesterday, by project: the commands, failures and what fixed them, agents
    Yesterday,
    /// What you ran today, by project
    Today,
    /// What you ran on a day: a weekday (monday) or a date (2026-09-28)
    Day { day: String },
    /// What your coding agents ran: runs, failures, time lost to failures, and commands an agent
    /// kept retrying the same way while they failed each time
    Agents {
        /// how many days back (default 7)
        #[arg(long, default_value_t = 7)]
        days: i64,
        /// print JSON
        #[arg(long)]
        json: bool,
    },
    /// Your coding agents' sessions, newest first, or the ones that ran what the words describe
    /// (`reman sessions alembic migration last week`): where, what they ran, how it went, and how
    /// to pick each one up again
    Sessions {
        words: Vec<String>,
        /// only this agent's (claude, codex, gemini, cursor, ...)
        #[arg(long)]
        agent: Option<String>,
        /// how many (default 10)
        #[arg(long, short = 'k', default_value_t = 10)]
        limit: i64,
        #[arg(long)]
        json: bool,
    },
    /// Reopen the agent session that ran what the words describe, in its folder (`reman resume
    /// alembic`); a number last picks another match, as numbered by `reman sessions`
    Resume {
        words: Vec<String>,
        /// only this agent's sessions
        #[arg(long)]
        agent: Option<String>,
        /// only print the command that resumes it
        #[arg(long)]
        print: bool,
    },
    /// Your shell inside reman's own terminal layer, so what your commands print is kept too
    /// (`reman output`, the finder's ^O). `reman shell pwsh`, `reman shell -- bash -l`; with no
    /// program, the shell you're in. Turn it on for every new shell in `reman settings`
    Shell {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        program: Vec<String>,
    },
    /// What commands printed: the newest runs whose output was kept, or the ones whose command is
    /// what the words describe, or whose output has the words (`reman output npm test`,
    /// `reman output --failed`, `reman output ECONNREFUSED`)
    Output {
        words: Vec<String>,
        /// only runs that failed
        #[arg(long)]
        failed: bool,
        /// how many (default 3)
        #[arg(long, short = 'k', default_value_t = 3)]
        limit: i64,
        /// every kept line, not just the last 40
        #[arg(long)]
        full: bool,
        #[arg(long)]
        json: bool,
    },
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
    /// Print shell integration: powershell | bash | zsh | fish | nu | xonsh | cmd (via Clink)
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
    /// reman's keys: in your shell (what opens the finder, inserts a fix, ...) and in the finder.
    /// `reman keys` lists them; `set <action> <key>...` or `set <action> off` changes one;
    /// `preset standard|gentle|vim` starts over from a preset; `reset` goes back to standard.
    /// Also on one page in `reman settings` (Keys). The finder and open shells take a change at
    /// once (shells at their next prompt; nushell, xonsh and Command Prompt in new terminals)
    Keys {
        /// list | set | preset | reset
        what: Option<String>,
        args: Vec<String>,
        /// print one shell's key bindings (what `reman init` puts in)
        #[arg(long)]
        print: Option<String>,
    },
    /// Usage statistics; with a period (today, week, month, year, yesterday, "last week", monday,
    /// 2026-09-28), what ran then: how it went, time spent, top commands and tools
    Stats { period: Vec<String> },
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
        Cmd::Search { query, k, here, cwd: dir, failed, worked, by, after, before, format, json: as_json, all_variants, semantic } => {
            let mut req = json!({"op": "search", "query": query.join(" "), "k": k, "group": !all_variants});
            if let Some(d) = dir.map(|d| full_path(&d)).or_else(|| here.then(cwd)) {
                req["cwd"] = json!(d);
            }
            if semantic {
                req["rank"] = json!("semantic");
            }
            if failed || worked {
                req["status"] = json!(if failed { "fail" } else { "ok" });
            }
            if let Some(a) = actor_arg(by.as_deref())? {
                req["actor"] = json!(a);
            }
            if let Some(a) = after {
                req["after"] = json!(a);
            }
            if let Some(b) = before {
                req["before"] = json!(b);
            }
            let t = Instant::now();
            let r = client::call(&req)?;
            let rows = results(&r);
            if as_json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
                return Ok(());
            }
            if let Some(f) = format {
                // one line per result, for scripts
                for r in &rows {
                    let by = r["actor"].as_str().and_then(|a| a.strip_prefix("agent:")).unwrap_or("you");
                    let line = f
                        .replace("{command}", r["command"].as_str().unwrap_or(""))
                        .replace("{folder}", r["cwd"].as_str().unwrap_or(""))
                        .replace("{runs}", &r["runs"].to_string())
                        .replace("{status}", r["status"].as_str().unwrap_or(""))
                        .replace("{last}", r["last_run"].as_str().unwrap_or(""))
                        .replace("{by}", by)
                        .replace("\\t", "\t");
                    println!("{line}");
                }
                return Ok(());
            }
            print_rows(&rows);
            eprintln!("\n\x1b[90m{} result(s), mode={}, {:.1} ms\x1b[0m", rows.len(), r["mode"].as_str().unwrap_or(""), t.elapsed().as_secs_f64() * 1000.0);
            Ok(())
        }
        Cmd::Delete { contains, regex, failed, by, before, cwd: dir, yes } => {
            let mut req = json!({"op": "delete", "failed": failed});
            if !contains.is_empty() {
                req["contains"] = json!(contains.join(" "));
            }
            if let Some(r) = regex {
                req["regex"] = json!(r);
            }
            if let Some(b) = before {
                req["before"] = json!(b);
            }
            if let Some(d) = dir {
                req["cwd"] = json!(full_path(&d));
            }
            if let Some(a) = actor_arg(by.as_deref())? {
                req["actor"] = json!(a);
            }
            remove_after_asking(req, yes)
        }
        Cmd::Prune { yes } => remove_after_asking(json!({"op": "prune"}), yes),
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
        Cmd::Goto { mut words } => {
            // a number last picks that match: `rcd alembic 2`, `rcd 3`
            let pick = words.last().and_then(|w| w.parse::<usize>().ok()).filter(|n| (1..=20).contains(n));
            if pick.is_some() {
                words.pop();
            }
            let r = client::call(&json!({"op": "goto", "query": words.join(" ")}))?;
            let found = r["results"].as_array().cloned().unwrap_or_default();
            let line = |n: usize, f: &Value| {
                let why = match (f["because"].as_str(), f["ago"].as_str()) {
                    (Some(b), Some(a)) => format!("   \x1b[90m({}, {a})\x1b[0m", b.lines().next().unwrap_or("")),
                    (None, Some(a)) => format!("   \x1b[90m(last here {a})\x1b[0m"),
                    _ => String::new(),
                };
                format!("{n}. {}{why}", f["folder"].as_str().unwrap_or(""))
            };
            if found.is_empty() {
                eprintln!("reman knows no folder for `{}`.", words.join(" "));
                std::process::exit(1);
            }
            // no words and no number: where you were lately, to pick from
            if words.is_empty() && pick.is_none() {
                for (i, f) in found.iter().enumerate() {
                    eprintln!("{}", line(i + 1, f));
                }
                eprintln!("\x1b[90mrcd <number> goes there\x1b[0m");
                std::process::exit(1);
            }
            let i = pick.unwrap_or(1) - 1;
            let Some(f) = found.get(i) else {
                eprintln!("only {} match{}.", found.len(), if found.len() == 1 { "" } else { "es" });
                std::process::exit(1);
            };
            eprintln!("\x1b[36m→\x1b[0m {}", line(i + 1, f).split_once(". ").map(|x| x.1).unwrap_or(""));
            for (j, g) in found.iter().enumerate().filter(|(j, _)| *j != i) {
                eprintln!("  \x1b[90m{}\x1b[0m", line(j + 1, g));
            }
            println!("{}", f["folder"].as_str().unwrap_or(""));
            Ok(())
        }
        Cmd::Yesterday => print_day("yesterday"),
        Cmd::Today => print_day("today"),
        Cmd::Day { day } => print_day(&day),
        Cmd::Agents { days, json: as_json } => {
            let r = client::call(&json!({"op": "agents", "days": days}))?;
            if as_json {
                println!("{}", serde_json::to_string_pretty(&r)?);
                return Ok(());
            }
            let actors = r["actors"].as_array().cloned().unwrap_or_default();
            println!("The last {} day{}:", days, if days == 1 { "" } else { "s" });
            if actors.is_empty() {
                println!("  nothing ran.");
                return Ok(());
            }
            for a in &actors {
                let (runs, failed, unknown) = (a["runs"].as_u64().unwrap_or(0), a["failed"].as_u64().unwrap_or(0), a["unknown"].as_u64().unwrap_or(0));
                let mut line = format!("  {:<14} {runs:>5} run{}", a["actor"].as_str().unwrap_or("?"), if runs == 1 { " " } else { "s" });
                if runs > unknown {
                    line.push_str(&format!("   {failed:>4} failed ({}%)", (failed * 100 + (runs - unknown) / 2) / (runs - unknown)));
                }
                if unknown > 0 {
                    line.push_str(&format!("   {unknown} with no result reported"));
                }
                let lost = a["lost_ms"].as_i64().unwrap_or(0) / 1000;
                if lost >= 60 {
                    line.push_str(&format!("   \x1b[90m{}m {}s spent on runs that failed\x1b[0m", lost / 60, lost % 60));
                }
                println!("{line}");
            }
            let loops = r["loops"].as_array().cloned().unwrap_or_default();
            if loops.is_empty() {
                println!("\nNo agent retried a command the same way 4+ times while it kept failing.");
            } else {
                println!("\nRetried the same way 4+ times, failing the same way each time:");
                for l in loops {
                    let c = l["command"].as_str().unwrap_or("").lines().next().unwrap_or("");
                    let c = if c.chars().count() > 60 { format!("{}…", c.chars().take(59).collect::<String>()) } else { c.to_string() };
                    let at = l["folder"].as_str().map(|f| format!(" in {f}")).unwrap_or_default();
                    println!("  \x1b[31m✗\x1b[0m {c}   \x1b[90m{} · {}x · {}{at}\x1b[0m", l["actor"].as_str().unwrap_or("?"), l["times"], l["ago"].as_str().unwrap_or(""));
                }
            }
            Ok(())
        }
        Cmd::Shell { program } => std::process::exit(pty::run(program)?),
        Cmd::Keys { what, args, print } => keys_cmd(what.as_deref(), &args, print.as_deref()),
        Cmd::Output { words, failed, limit, full, json: as_json } => {
            let r = client::call(&json!({"op": "outputs", "query": words.join(" "), "failed": failed, "k": limit}))?;
            if as_json {
                println!("{}", serde_json::to_string_pretty(&r)?);
                return Ok(());
            }
            let list = r["results"].as_array().cloned().unwrap_or_default();
            if list.is_empty() {
                println!(
                    "{}",
                    match (words.is_empty(), failed) {
                        (true, false) => "No output kept yet. Agents' is kept as they run; yours inside `reman shell` (reman settings turns it on for every shell).",
                        (true, true) => "No failed run's output kept yet.",
                        (false, _) => "No kept output for anything like that.",
                    }
                );
                return Ok(());
            }
            for x in &list {
                let (g, c) = match x["exit"].as_i64() {
                    Some(0) => ("✓", "32"),
                    Some(_) => ("✗", "31"),
                    None => ("·", "90"),
                };
                let exit = x["exit"].as_i64().filter(|e| *e != 0).map(|e| format!(" · exit {e}")).unwrap_or_default();
                let at = x["folder"].as_str().map(|f| format!(" · in {f}")).unwrap_or_default();
                println!("\x1b[{c}m{g}\x1b[0m \x1b[1m{}\x1b[0m   \x1b[90m{} ({}){exit} · {}{at}\x1b[0m", x["command"].as_str().unwrap_or("").lines().next().unwrap_or(""), x["ago"].as_str().unwrap_or(""), x["when"].as_str().unwrap_or(""), x["by"].as_str().unwrap_or(""));
                let lines: Vec<&str> = x["output"].as_str().unwrap_or("").lines().collect();
                let from = if full { 0 } else { lines.len().saturating_sub(40) };
                if from > 0 {
                    println!("  \x1b[90m│ … {from} lines before (--full shows them)\x1b[0m");
                }
                for l in &lines[from..] {
                    println!("  \x1b[90m│\x1b[0m {l}");
                }
                println!();
            }
            Ok(())
        }
        Cmd::Sessions { words, agent, limit, json: as_json } => {
            let r = client::call(&json!({"op": "sessions", "query": words.join(" "), "agent": agent, "k": limit}))?;
            if as_json {
                println!("{}", serde_json::to_string_pretty(&r)?);
                return Ok(());
            }
            print_sessions(&r);
            Ok(())
        }
        Cmd::Resume { mut words, agent, print } => {
            // a number last picks the nth match
            let pick = words.last().and_then(|w| w.parse::<usize>().ok()).filter(|n| (1..=50).contains(n));
            if pick.is_some() {
                words.pop();
            }
            let pick = pick.unwrap_or(1);
            let r = client::call(&json!({"op": "sessions", "query": words.join(" "), "agent": agent, "k": pick.max(10)}))?;
            let list = r["results"].as_array().cloned().unwrap_or_default();
            let Some(x) = list.get(pick - 1) else {
                match (words.is_empty(), list.len()) {
                    (true, 0) => println!("No agent sessions recorded yet."),
                    (false, 0) => println!("No agent session ran anything like \"{}\".", words.join(" ")),
                    (_, n) => println!("There {} only {n} match{}.", if n == 1 { "is" } else { "are" }, if n == 1 { "" } else { "es" }),
                }
                std::process::exit(1);
            };
            let agent = x["agent"].as_str().unwrap_or("?");
            let folder = x["folder"].as_str().map(config::native_path);
            let argv: Vec<String> = x["resume_argv"].as_array().into_iter().flatten().filter_map(|a| a.as_str().map(String::from)).collect();
            if argv.is_empty() {
                println!("{agent}'s sessions can't be reopened from a terminal; it was session {} ({}, in {}).", x["session"].as_str().unwrap_or("?"), x["ago"].as_str().unwrap_or("?"), folder.as_deref().unwrap_or("?"));
                std::process::exit(1);
            }
            if print {
                println!("{}", argv.join(" "));
                return Ok(());
            }
            let dir = folder.filter(|f| std::path::Path::new(f).is_dir());
            eprintln!(
                "\x1b[90mreman: {agent}'s session from {} ({} commands{}), in {}\x1b[0m",
                x["ago"].as_str().unwrap_or("?"),
                x["runs"],
                if x["failed"].as_u64().unwrap_or(0) > 0 { format!(", {} failed", x["failed"]) } else { String::new() },
                dir.as_deref().unwrap_or("this folder (its own is gone)")
            );
            eprintln!("\x1b[90m       {}\x1b[0m", argv.join(" "));
            // npm installs these as .cmd scripts on Windows, which only cmd starts
            let mut cmd = if cfg!(windows) {
                let mut c = std::process::Command::new("cmd");
                c.arg("/C").args(&argv);
                c
            } else {
                let mut c = std::process::Command::new(&argv[0]);
                c.args(&argv[1..]);
                c
            };
            if let Some(d) = &dir {
                cmd.current_dir(d);
            }
            let st = cmd.status().with_context(|| format!("could not start {}", argv[0]))?;
            std::process::exit(st.code().unwrap_or(1));
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
            if let Some(a) = r["git"]["advice"].as_str() {
                println!("{a}");
            }
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
            let mut branch: Option<&str> = None;
            for t in r["timeline"].as_array().into_iter().flatten() {
                // the branch, where it changes
                let b = t["branch"].as_str();
                let on = match b {
                    Some(b) if branch.is_some_and(|p| p != b) => format!("   \x1b[33mon {b}\x1b[0m"),
                    _ => String::new(),
                };
                if b.is_some() {
                    branch = b;
                }
                let mark = match t["exit"].as_i64() {
                    Some(0) => "\x1b[32m✓\x1b[0m",
                    Some(_) => "\x1b[31m✗\x1b[0m",
                    None => "·",
                };
                let c = t["command"].as_str().unwrap_or("").lines().next().unwrap_or("");
                let bold = if t["target"] == json!(true) { "\x1b[1m" } else { "" };
                println!("  {mark} {bold}{c}\x1b[0m   \x1b[90m{}\x1b[0m{on}", t["ago"].as_str().unwrap_or(""));
            }
            Ok(())
        }
        Cmd::Runbook { json: as_json, no_ai, refresh } => {
            let mut r = client::call(&json!({"op": "runbook", "cwd": cwd(), "fresh": no_ai || refresh}))?;
            let has_unplaced = r["unplaced"].as_array().is_some_and(|a| !a.is_empty());
            // a language model, if one is available, arranges and explains (never invents)
            if !no_ai && r.get("model").is_none() && (r["found"] == json!(true) || has_unplaced) {
                match ai::find() {
                    Some(m) => {
                        eprintln!("\x1b[90mreman: asking {} to arrange the runbook (--static skips this)...\x1b[0m", m.label());
                        match ai::runbook(&m, &r) {
                            Ok(x) => {
                                let _ = ai::save(&r, &x, &m.model);
                                r = ai::merge(&r, &x, &m.model);
                            }
                            Err(e) => eprintln!("\x1b[90mreman: the model didn't answer usefully ({e:#}); here is the runbook from your history alone.\x1b[0m"),
                        }
                    }
                    None if !as_json && !ai::turned_off() => eprintln!(
                        "\x1b[90mreman: no language model is running here (Ollama, LM Studio or llama.cpp). With one, the runbook also gets a summary and a getting-started order.\x1b[0m"
                    ),
                    None => {}
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
            // where a command runs, when that isn't the folder you're in
            let here = r["here"].as_str().unwrap_or(".");
            let place = |dir: &str| match dir {
                d if d == here => String::new(),
                "." => " in the project root".to_string(),
                d => format!(" in `{d}/`"),
            };
            let dir_of = |cmd: &str| {
                let mut all = r["sections"].as_array().into_iter().flatten().flat_map(|s| s["commands"].as_array().into_iter().flatten());
                all.find(|c| c["command"] == json!(cmd)).and_then(|c| c["dir"].as_str()).unwrap_or(here).to_string()
            };
            // a command typed over several lines (`` ` `` / `\` continuations) as the one line it is
            let flat = |s: &str| s.lines().map(|l| l.trim().trim_end_matches(['`', '\\']).trim_end()).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ");
            let first: Vec<&str> = r["getting_started"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            if !first.is_empty() {
                println!("## Getting started\n");
                for (n, c) in first.iter().enumerate() {
                    println!("{}. `{}`{}", n + 1, flat(c), place(&dir_of(c)));
                }
                println!();
            }
            for sec in r["sections"].as_array().into_iter().flatten() {
                println!("## {}\n", sec["title"].as_str().unwrap_or(""));
                for c in sec["commands"].as_array().into_iter().flatten() {
                    // only what was seen: a run whose result a pipe hid proves nothing
                    let n = |k: &str| c[k].as_u64().unwrap_or(0);
                    let (runs, ok, fail, unseen) = (n("runs"), n("worked"), n("failed"), n("unseen"));
                    let rec = match (ok, fail, unseen) {
                        (0, 0, _) => format!("ran {runs}x, result not seen"),
                        (_, 0, 0) => format!("worked {ok}x"),
                        (_, _, 0) => format!("worked {ok} of {runs}"),
                        (_, 0, u) => format!("worked {ok}x (+{u} runs, result not seen)"),
                        (_, f, u) => format!("worked {ok} of {} (+{u} runs, result not seen)", ok + f),
                    };
                    let rec = match c["declared"].as_str() {
                        Some(f) => format!("in {f}, not run yet"),
                        None => format!("{rec} · last {}", c["last_run"].as_str().unwrap_or("?")),
                    };
                    let note = c["note"].as_str().map(|n| format!(": {n}")).unwrap_or_default();
                    let at = place(c["dir"].as_str().unwrap_or(here));
                    let part = if c["partial"] == json!(true) { " (some files only)" } else { "" };
                    println!("- `{}`{at}{part}{note} · {rec}", flat(c["command"].as_str().unwrap_or("")));
                }
                println!();
            }
            let flows: Vec<&Value> = r["flows"].as_array().into_iter().flatten().collect();
            if !flows.is_empty() {
                println!("## Usual sequences\n");
                for f in flows {
                    let steps: Vec<String> = f["steps"].as_array().into_iter().flatten().filter_map(Value::as_str).map(|s| format!("`{}`", flat(s))).collect();
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
            "cursor" => capture::hook_cursor(),
            "gemini" => capture::hook_gemini(),
            "windsurf" => capture::hook_windsurf(),
            "copilot" => capture::hook_copilot(),
            "event" => capture::hook_event(),
            other => bail!("unknown agent hook {other:?} (supported: claude, codex, cursor, gemini, windsurf, copilot, event)"),
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
        Cmd::Stats { period } if !period.is_empty() && period.join(" ") != "all" => {
            let p = period.join(" ");
            let s = client::call(&json!({"op": "stats", "period": p}))?;
            if let Some(e) = s["error"].as_str() {
                bail!("{e}");
            }
            let runs = s["runs"].as_u64().unwrap_or(0);
            if runs == 0 {
                println!("{p}: nothing ran.");
                return Ok(());
            }
            let (ok, fail) = (s["ok_runs"].as_u64().unwrap_or(0), s["failed_runs"].as_u64().unwrap_or(0));
            let worked = if ok + fail > 0 { format!("{}% worked", (ok * 100 + (ok + fail) / 2) / (ok + fail)) } else { "outcomes not recorded".into() };
            let took = |ms: i64| crate::insight::took(ms.clamp(0, u32::MAX as i64) as u32, false);
            println!("\x1b[1m{p}\x1b[0m: {runs} runs of {} commands · {worked} · you {}, agents {}", s["commands"], s["you"], s["agents"]);
            let ms = s["time_ms"].as_i64().unwrap_or(0);
            if ms >= 1000 {
                println!("  time spent running commands: {} (on runs that failed: {})", took(ms), took(s["failed_time_ms"].as_i64().unwrap_or(0)));
            }
            let short = |c: &str| {
                let c = c.lines().next().unwrap_or("");
                if c.chars().count() > 60 { format!("{}…", c.chars().take(59).collect::<String>()) } else { c.to_string() }
            };
            let tools: Vec<String> = s["tools"].as_array().into_iter().flatten().map(|t| format!("{} ({})", t["tool"].as_str().unwrap_or(""), t["runs"])).collect();
            if !tools.is_empty() {
                println!("\n  most used: {}", tools.join(" · "));
            }
            for (title, key) in [("most run", "top"), ("failed most", "failing"), ("busiest folders", "folders")] {
                let rows = s[key].as_array().cloned().unwrap_or_default();
                if rows.is_empty() {
                    continue;
                }
                println!("\n  {title}:");
                for t in rows {
                    println!("    {:>5}  {}", t["runs"], short(t["command"].as_str().unwrap_or("")));
                }
            }
            Ok(())
        }
        Cmd::Stats { .. } => {
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
            let a = mcp::Answer::from_reply(mcp::Bridge::new(mcp::Policy::from_env()).call(&tool, &args)?);
            println!("{}", serde_json::to_string_pretty(&a.value)?);
            if let Some(n) = a.note {
                println!("note: {n}");
            }
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
        "nu" | "nushell" => include_str!("init/reman.nu"),
        "xonsh" => include_str!("init/reman.xsh"),
        other => bail!("unknown shell {other:?} (powershell|bash|zsh|fish|nu|xonsh|cmd)"),
    };
    let fix = |p: &Path| {
        let s = p.to_string_lossy().into_owned();
        if matches!(shell, "powershell" | "pwsh" | "cmd" | "clink") { s } else { s.replace('\\', "/") }
    };
    // shells open inside `reman shell` (reman settings): a PowerShell boolean, a number elsewhere
    let layer = settings::load().shell_layer;
    let layer = match shell {
        "powershell" | "pwsh" => if layer { "$true" } else { "$false" },
        _ => if layer { "1" } else { "0" },
    };
    // reman's keys, as the user has them (`reman settings`, Keys), in this shell's own words
    let map = keys::Map::load();
    Ok(tpl
        .replace("__KEYS__", &keys::bindings(shell, &map))
        .replace("__FIX_HINT__", &keys::fix_hint(&map))
        .replace("__NEXT_KEY__", &map.label("next").unwrap_or_else(|| "it".into()))
        .replace("__CONFIG__", &fix(&settings::path()))
        .replace("__SHELL_LAYER__", layer)
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

/// `--by`'s values, for checking and for Tab completion (`me`, `human` and `agent` are accepted too).
fn by_values() -> clap::builder::PossibleValuesParser {
    use clap::builder::PossibleValue;
    clap::builder::PossibleValuesParser::new([
        PossibleValue::new("you").help("what you ran").aliases(["me", "human"]),
        PossibleValue::new("agents").help("what your coding agents ran").alias("agent"),
    ])
}

/// `--by you|agents` as the daemon's actor filter.
fn actor_arg(by: Option<&str>) -> Result<Option<&'static str>> {
    Ok(match by {
        None => None,
        Some("you" | "me" | "human") => Some("human"),
        Some("agents" | "agent") => Some("agent"),
        Some(o) => bail!("--by {o}: use `you` or `agents`"),
    })
}

/// A folder given on the command line, as an absolute path (the daemon knows folders that way).
fn full_path(d: &str) -> String {
    std::fs::canonicalize(d)
        .map(|p| {
            let s = p.to_string_lossy().into_owned();
            s.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(s)
        })
        .unwrap_or_else(|_| d.to_string())
}

/// `reman delete` / `reman prune`: list what matches, ask, then remove it.
fn remove_after_asking(mut req: Value, yes: bool) -> Result<()> {
    let r = client::call(&req)?;
    if let Some(e) = r["error"].as_str() {
        bail!("{e}");
    }
    let list = r["commands"].as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        println!("Nothing matches: nothing to remove.");
        return Ok(());
    }
    for c in list.iter().take(30) {
        let line = c["command"].as_str().unwrap_or("").lines().next().unwrap_or("");
        let at = c["folder"].as_str().map(|f| format!("   \x1b[90min {f}\x1b[0m")).unwrap_or_default();
        let why = c["why"].as_str().map(|w| format!("   \x1b[90m({w})\x1b[0m")).unwrap_or_default();
        println!("  {line}   \x1b[90m{}x\x1b[0m{at}{why}", c["runs"]);
    }
    if list.len() > 30 {
        println!("  \x1b[90m... and {} more\x1b[0m", list.len() - 30);
    }
    if !yes {
        use std::io::Write;
        print!("\nRemove these {} from your history? This can't be undone. [y/N] ", list.len());
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("Nothing removed.");
            return Ok(());
        }
    }
    req["apply"] = json!(true);
    let r = client::call(&req)?;
    println!("Removed {} from your history.", r["deleted"]);
    Ok(())
}

/// `reman yesterday` / `today` / `day <date>`: the day's story, by project.
/// `reman keys`: list the keys, change one, pick a preset, or start over.
fn keys_cmd(what: Option<&str>, args: &[String], print: Option<&str>) -> Result<()> {
    if let Some(shell) = print {
        println!("{}", keys::bindings(shell, &keys::Map::load()));
        return Ok(());
    }
    let mut st = settings::load();
    match what.unwrap_or("list") {
        "list" => {}
        "set" => {
            let Some((id, rest)) = args.split_first() else { bail!("reman keys set <action> <key>... (or off): actions are listed by `reman keys`") };
            let Some(a) = keys::action(id) else { bail!("`{id}` isn't an action: `reman keys` lists them") };
            let off = rest.is_empty() || (rest.len() == 1 && rest[0].eq_ignore_ascii_case("off"));
            let mut chosen: Vec<String> = Vec::new();
            if !off {
                let map = keys::Map::of(st.key_preset.as_deref(), &st.keys);
                for k in rest {
                    let key = keys::Key::parse(k)?;
                    match keys::check(&map, a.id, &key) {
                        Some((true, why)) => bail!("{}: {why}", key.label()),
                        Some((false, why)) => println!("  note: {} - {why}", key.label()),
                        None => {}
                    }
                    chosen.push(key.label());
                }
            }
            st.keys.insert(a.id.to_string(), chosen);
            settings::save(&st)?;
        }
        "preset" => {
            let Some(p) = args.first().filter(|p| keys::PRESETS.iter().any(|x| x.0 == p.as_str())) else {
                bail!("reman keys preset standard|gentle|vim")
            };
            st.key_preset = (p != "standard").then(|| p.clone());
            st.finder_keys = (p == "vim").then(|| "vim".to_string());
            st.keys.clear();
            settings::save(&st)?;
        }
        "reset" => {
            st.key_preset = None;
            st.keys.clear();
            settings::save(&st)?;
        }
        other => bail!("reman keys {other}: use list, set, preset or reset"),
    }
    let st = settings::load();
    let map = keys::Map::of(st.key_preset.as_deref(), &st.keys);
    println!("preset: {}{}", map.preset, if st.keys.is_empty() { String::new() } else { format!(", with {} change(s)", st.keys.len()) });
    for (layer, title) in [(keys::Layer::Shell, "in your shell"), (keys::Layer::Finder, "in the finder")] {
        println!("\n{title}");
        for a in keys::ACTIONS.iter().filter(|a| a.layer == layer) {
            let k: Vec<String> = map.get(a.id).iter().map(keys::Key::label).collect();
            let shown = if k.is_empty() { "\x1b[90moff\x1b[0m".to_string() } else { k.join(", ") };
            let changed = if st.keys.contains_key(a.id) { " \x1b[33m*\x1b[0m" } else { "" };
            println!("  {:<12} {:<18}{changed} \x1b[90m{}\x1b[0m", a.id, shown, a.what);
        }
    }
    if what.is_some_and(|w| w != "list") {
        println!("\nThe finder uses these now; open shells at their next prompt (nushell, xonsh and Command Prompt: new terminals).");
    }
    Ok(())
}

fn print_sessions(r: &Value) {
    let list = r["results"].as_array().cloned().unwrap_or_default();
    let words = r["query"].as_str().unwrap_or("");
    if list.is_empty() {
        match (words.is_empty(), r["window"].as_str()) {
            (true, None) => println!("No agent sessions recorded yet. Connect an agent with `reman connect`."),
            (true, Some(w)) => println!("No agent session ran anything {w}."),
            (false, _) => println!("No agent session ran anything like \"{words}\"."),
        }
        return;
    }
    // what was asked, time words included, for `reman resume` to ask the same
    let asked: String = [Some(words), r["window"].as_str()].into_iter().flatten().filter(|w| !w.is_empty()).map(|w| format!(" {w}")).collect();
    let short = |c: &str| {
        let c = c.lines().next().unwrap_or("");
        if c.chars().count() > 46 { format!("{}…", c.chars().take(45).collect::<String>()) } else { c.to_string() }
    };
    for (i, x) in list.iter().enumerate() {
        let (runs, failed) = (x["runs"].as_u64().unwrap_or(0), x["failed"].as_u64().unwrap_or(0));
        let fail = if failed > 0 { format!(", \x1b[31m{failed} failed\x1b[0m") } else { String::new() };
        println!(
            "\x1b[1m{}\x1b[0m  {}   \x1b[90m{} · {} to {} ({})\x1b[0m · {runs} command{}{fail}",
            i + 1,
            x["agent"].as_str().unwrap_or("?"),
            x["ago"].as_str().unwrap_or(""),
            x["from"].as_str().unwrap_or(""),
            x["to"].as_str().unwrap_or(""),
            x["took"].as_str().unwrap_or(""),
            if runs == 1 { "" } else { "s" },
        );
        if let Some(f) = x["folder"].as_str() {
            println!("   {f}");
        }
        let flow: Vec<String> = x["flow"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|f| {
                let n = f["times"].as_u64().unwrap_or(1);
                format!("{}{}", short(f["command"].as_str().unwrap_or("")), if n > 1 { format!(" ({n}x)") } else { String::new() })
            })
            .collect();
        if !flow.is_empty() {
            let more = x["more"].as_u64().unwrap_or(0);
            println!("   {}{}", flow.join(" → "), if more > 0 { format!("  \x1b[90m(+{more} more)\x1b[0m") } else { String::new() });
        }
        if !words.is_empty() {
            let m: Vec<String> = x["matched"].as_array().into_iter().flatten().filter_map(Value::as_str).map(|c| short(c)).collect();
            if !m.is_empty() {
                println!("   \x1b[90mran:\x1b[0m \x1b[36m{}\x1b[0m", m.join("  ·  "));
            }
        }
        match x["resume"].as_str() {
            Some(c) => println!("   \x1b[90mresume:\x1b[0m {c}   \x1b[90m(or reman resume{asked}{})\x1b[0m", if i == 0 { String::new() } else { format!(" {}", i + 1) }),
            None => println!("   \x1b[90msession {} (opened from {}, not a terminal)\x1b[0m", x["session"].as_str().unwrap_or("?"), x["agent"].as_str().unwrap_or("its app")),
        }
        println!();
    }
    let (total, shown) = (r["total"].as_u64().unwrap_or(0), list.len() as u64);
    if total > shown {
        println!("\x1b[90m{} more; -k {} shows them\x1b[0m", total - shown, total.min(50));
    }
}

fn print_day(day: &str) -> Result<()> {
    let r = client::call(&json!({"op": "day", "day": day}))?;
    if let Some(reason) = r["reason"].as_str() {
        println!("{reason}");
        return Ok(());
    }
    let title = match day {
        "today" => "Today".to_string(),
        "yesterday" => "Yesterday".to_string(),
        d => d.to_string(),
    };
    if r["found"] != json!(true) {
        println!("{title}: nothing ran.");
        return Ok(());
    }
    let short = |c: &str| {
        let c = c.lines().next().unwrap_or("");
        if c.chars().count() > 50 { format!("{}…", c.chars().take(49).collect::<String>()) } else { c.to_string() }
    };
    println!("\x1b[1m{title}\x1b[0m, {} to {}", r["from"].as_str().unwrap_or("?"), r["to"].as_str().unwrap_or("?"));
    for p in r["projects"].as_array().into_iter().flatten() {
        let branch = p["branch"].as_str().map(|b| format!("  ({b})")).unwrap_or_default();
        println!("\n\x1b[1m{}\x1b[0m{branch}   \x1b[90m{} to {}\x1b[0m", p["name"].as_str().unwrap_or("?"), p["from"].as_str().unwrap_or(""), p["to"].as_str().unwrap_or(""));
        let flow: Vec<String> = p["flow"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|f| {
                let n = f["times"].as_u64().unwrap_or(1);
                format!("{}{}", short(f["command"].as_str().unwrap_or("")), if n > 1 { format!(" ({n}x)") } else { String::new() })
            })
            .collect();
        if !flow.is_empty() {
            let more = p["more"].as_u64().unwrap_or(0);
            println!("  {}{}", flow.join(" → "), if more > 0 { format!("  \x1b[90m(+{more} more)\x1b[0m") } else { String::new() });
        }
        for f in p["failures"].as_array().into_iter().flatten() {
            let c = short(f["command"].as_str().unwrap_or(""));
            let n = f["times"].as_u64().unwrap_or(1);
            if let Some(fix) = f["fixed_by"].as_str() {
                println!("  \x1b[31m✗\x1b[0m {c}  →  \x1b[32m✓\x1b[0m {}   \x1b[90m(fixed)\x1b[0m", short(fix));
            } else if f["worked_later"] == json!(true) {
                println!("  \x1b[31m✗\x1b[0m {c}   \x1b[90m(worked later)\x1b[0m");
            } else {
                println!("  \x1b[31m✗\x1b[0m {c}   \x1b[33mstill failing\x1b[0m{}", if n > 1 { format!(" ({n} failures)") } else { String::new() });
            }
        }
        for a in p["agents"].as_array().into_iter().flatten() {
            let (runs, failed) = (a["runs"].as_u64().unwrap_or(0), a["failed"].as_u64().unwrap_or(0));
            let f = if failed > 0 { format!(" ({failed} failed)") } else { String::new() };
            println!("  \x1b[90m{} ran {runs} command{} here{f}\x1b[0m", a["agent"].as_str().unwrap_or("?"), if runs == 1 { "" } else { "s" });
        }
    }
    Ok(())
}

fn connect_cmd(targets: &[String], roots: &[String], add: &[String], remove: &[String], port: Option<u16>, print: bool, old_history: Option<&str>) -> Result<()> {
    let (exe, hook) = agent_exes()?;
    if print {
        println!("{}", connect::generic_snippet(&exe));
        return Ok(());
    }
    // the boundary: one list of folders for every agent, and only the user widens it
    if !roots.is_empty() || !add.is_empty() || old_history == Some("on") {
        connect::refuse_if_agent("Sharing history with agents")?;
    }
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
    let rcs: Vec<PathBuf> = [user_home.join(".bashrc"), user_home.join(".zshrc"), user_home.join(".config").join("fish").join("config.fish"), user_home.join(".xonshrc")]
        .into_iter()
        .filter(|p| std::fs::read_to_string(p).is_ok_and(|t| t.contains("# reman shell integration")))
        .collect();
    plan.extend(rcs.iter().map(|p| format!("shell           reman's line in {}", p.display())));
    let nu_file = Some(nu_autoload_file(&user_home)).filter(|f| f.exists());
    if let Some(f) = &nu_file {
        plan.push(format!("nushell         {}", f.display()));
    }
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
    if let Some(f) = &nu_file {
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
    // 5c. nushell and xonsh, wherever they're installed
    if !no_profile {
        let home = dirs::home_dir().context("no home folder")?;
        if connect::on_path("nu") {
            let f = nu_autoload_file(&home);
            if let Some(d) = f.parent() {
                std::fs::create_dir_all(d)?;
            }
            std::fs::write(&f, init_script("nu", &installed)?)?;
            println!("  shell (nushell) : WIRED {} -> open a new nushell", f.display());
        }
        if connect::on_path("xonsh") {
            match wire_xonsh(&installed, &home) {
                Ok((rc, true)) => println!("  shell (xonsh)   : WIRED {} -> open a new xonsh", rc.display()),
                Ok((rc, false)) => println!("  shell (xonsh)   : already wired ({})", rc.display()),
                Err(e) => println!("  shell (xonsh)   : {e:#}"),
            }
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

/// The file nushell loads on start: `$nu.data-dir/vendor/autoload/reman.nu`, its data folder
/// being the OS's (%APPDATA%\nushell, ~/.local/share/nushell, ~/Library/Application Support/nushell).
/// Written whole by `reman setup` (nushell sources files by a fixed path, not a command's output).
fn nu_autoload_file(home: &Path) -> PathBuf {
    let data = dirs::data_dir().filter(|_| dirs::home_dir().as_deref() == Some(home)).unwrap_or_else(|| {
        if cfg!(windows) {
            home.join("AppData").join("Roaming")
        } else if cfg!(target_os = "macos") {
            home.join("Library").join("Application Support")
        } else {
            home.join(".local").join("share")
        }
    });
    data.join("nushell").join("vendor").join("autoload").join("reman.nu")
}

/// xonsh: add `execx($(reman init xonsh))` to ~/.xonshrc, once. Returns (rc file, added now).
fn wire_xonsh(exe: &Path, home: &Path) -> Result<(PathBuf, bool)> {
    const MARK: &str = "# reman shell integration";
    let rc = home.join(".xonshrc");
    let text = std::fs::read_to_string(&rc).unwrap_or_default();
    if text.contains(MARK) {
        return Ok((rc, false));
    }
    let exe = exe.to_string_lossy().replace('\\', "/");
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&rc)?;
    use std::io::Write as _;
    writeln!(f, "\nexecx($(@({exe:?}) init xonsh))  {MARK}")?;
    Ok((rc, true))
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
    fn xonsh_wired_once_and_unwired() {
        let home = std::env::temp_dir().join(format!("reman-xonsh-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join(".xonshrc"), "$PROMPT = '> '\n").unwrap();
        let exe = Path::new(r"C:\Users\u\.reman\bin\reman.exe");
        assert!(wire_xonsh(exe, &home).unwrap().1);
        assert!(!wire_xonsh(exe, &home).unwrap().1, "once");
        let rc = std::fs::read_to_string(home.join(".xonshrc")).unwrap();
        assert!(rc.contains(r#"execx($(@("C:/Users/u/.reman/bin/reman.exe") init xonsh))"#), "{rc}");
        assert_eq!(unwire_rc(&rc).unwrap(), "$PROMPT = '> '\n");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn init_templates_substitute() {
        for sh in ["powershell", "bash", "zsh", "fish", "nu", "xonsh", "cmd"] {
            let s = init_script(sh, Path::new("/x/reman")).unwrap();
            assert!(!s.contains("__REMAN__") && !s.contains("__PORT__") && !s.contains("__SPOOL__") && !s.contains("__HOOK__") && !s.contains("__SHELL_LAYER__")
                    && !s.contains("__KEYS__") && !s.contains("__FIX_HINT__") && !s.contains("__NEXT_KEY__") && !s.contains("__CONFIG__"), "{sh}");
        }
    }
}
