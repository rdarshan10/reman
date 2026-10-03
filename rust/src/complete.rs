//! Tab completion for `reman` itself - ONE engine for every shell. The glue in each init script
//! passes the words typed so far plus the partial word under the cursor; this prints
//! `value<TAB>description` lines. Values come from the CLI definition (subcommands, flags) and
//! from live context: which agents are installed/connected, folders you could share with agents,
//! your own commands for forget / pin / check / fixes, the agent tools for `call`.
use crate::client::Client;
use crate::{config, connect, settings};
use clap::{Arg, Command};
use serde_json::{Value, json};
use std::time::Duration;

pub struct Cand {
    pub value: String,
    pub help: String,
}

fn cand(v: impl Into<String>, h: impl Into<String>) -> Cand {
    Cand { value: v.into(), help: h.into() }
}

fn takes_value(a: &Arg) -> bool {
    a.get_action().takes_values() && !a.is_positional()
}

/// What completes at the cursor, given the finished words before it and the partial one.
pub fn candidates(root: &Command, done: &[String], cur: &str) -> Vec<Cand> {
    let mut sub = root;
    let mut pending: Option<String> = None; // long name of an option waiting for its value
    let mut positionals = 0usize;
    let mut raw = false;
    for w in done {
        if pending.take().is_some() {
            continue;
        }
        if w == "--" {
            raw = true;
            continue;
        }
        if !raw && w.starts_with("--") {
            let name = w.trim_start_matches("--").split('=').next().unwrap_or("");
            if let Some(a) = sub.get_arguments().find(|a| a.get_long() == Some(name)) {
                if takes_value(a) && !w.contains('=') {
                    pending = Some(name.to_string());
                }
            }
            continue;
        }
        if !raw && w.len() == 2 && w.starts_with('-') {
            let c = w.chars().nth(1).unwrap_or(' ');
            if let Some(a) = sub.get_arguments().find(|a| a.get_short() == Some(c)) {
                if takes_value(a) {
                    pending = a.get_long().map(String::from).or(Some(c.to_string()));
                }
            }
            continue;
        }
        if positionals == 0 && std::ptr::eq(sub, root) {
            if let Some(s) = root.get_subcommands().find(|s| s.get_name() == w) {
                sub = s;
                continue;
            }
        }
        positionals += 1;
    }
    let name = if std::ptr::eq(sub, root) { "" } else { sub.get_name() };
    let flags = || -> Vec<Cand> {
        let mut v: Vec<Cand> = sub
            .get_arguments()
            .filter(|a| !a.is_hide_set() && !a.is_positional())
            .filter_map(|a| a.get_long().map(|l| cand(format!("--{l}"), a.get_help().map(|h| h.to_string()).unwrap_or_default())))
            .collect();
        v.push(cand("--help", "show help"));
        v
    };
    let mut out = if let Some(opt) = pending {
        option_values(sub, name, &opt, cur)
    } else if cur.starts_with('-') {
        flags()
    } else if name.is_empty() {
        root.get_subcommands().filter(|s| !s.is_hide_set()).map(|s| cand(s.get_name(), s.get_about().map(|a| a.to_string()).unwrap_or_default())).collect()
    } else {
        let mut v = positional_values(name, positionals, cur);
        if v.is_empty() && cur.is_empty() {
            v = flags();
        }
        v
    };
    // prefix match (case-insensitive); history commands also match anywhere in the text
    let c = cur.trim_start_matches(['"', '\'']).to_lowercase();
    let loose = matches!(name, "forget" | "pin" | "check" | "fixes");
    out.retain(|x| {
        let v = x.value.to_lowercase();
        v.starts_with(&c) || (loose && v.contains(&c))
    });
    out
}

fn option_values(sub: &Command, name: &str, opt: &str, cur: &str) -> Vec<Cand> {
    match (name, opt) {
        ("connect", "root" | "add-root") => {
            let st = settings::load();
            let mut v: Vec<Cand> = unshared_folders(&st.mcp_roots).into_iter().map(|(f, n)| cand(f, format!("{n} runs, not shared with agents"))).collect();
            v.extend(dirs(cur));
            v
        }
        ("connect", "remove-root") => settings::load().mcp_roots.into_iter().map(|r| cand(r, "shared with agents")).collect(),
        ("connect", "old-history") => vec![cand("on", "share generic old commands with agents"), cand("off", "keep folder-less history private")],
        ("tools", "format") => ["mcp", "openai", "openai-responses", "anthropic"].iter().map(|f| cand(*f, "tool schema format")).collect(),
        ("import", "path") | ("find", "cwd") | ("find", "result-file") => dirs(cur),
        ("find", "scope") => ["folder", "repo", "all"].iter().map(|s| cand(*s, "")).collect(),
        _ => sub
            .get_arguments()
            .find(|a| a.get_long() == Some(opt))
            .map(|a| a.get_possible_values().iter().map(|p| cand(p.get_name(), p.get_help().map(|h| h.to_string()).unwrap_or_default())).collect())
            .unwrap_or_default(),
    }
}

fn positional_values(name: &str, pos: usize, cur: &str) -> Vec<Cand> {
    match (name, pos) {
        ("connect", _) | ("disconnect", _) => agents(name == "disconnect"),
        ("init", 0) => [("powershell", "PowerShell"), ("bash", "bash"), ("zsh", "zsh"), ("fish", "fish"), ("nu", "nushell"), ("xonsh", "xonsh"), ("cmd", "Command Prompt (through Clink)")]
            .iter()
            .map(|(s, h)| cand(*s, format!("print the {h} integration")))
            .collect(),
        ("import", 0) => [("atuin", "Atuin's history.db"), ("psreadline", "PowerShell history"), ("bash", "~/.bash_history"), ("zsh", "~/.zsh_history"), ("fish", "fish history")]
            .iter()
            .map(|(s, h)| cand(*s, *h))
            .collect(),
        ("call", 0) => crate::mcp::tools()
            .as_array()
            .map(|a| a.iter().map(|t| cand(t["name"].as_str().unwrap_or(""), first_sentence(t["description"].as_str().unwrap_or("")))).collect())
            .unwrap_or_default(),
        ("stats", 0) => [("today", "what ran today"), ("yesterday", "what ran yesterday"), ("week", "this week"), ("month", "this month"), ("year", "this year")]
            .iter()
            .map(|(s, h)| cand(*s, *h))
            .collect(),
        ("forget" | "pin" | "check", 0) => history(cur, None),
        ("fixes", 0) => history(cur, Some("fail")),
        _ => vec![],
    }
}

fn first_sentence(s: &str) -> String {
    s.split(". ").next().unwrap_or(s).chars().take(80).collect()
}

fn agents(connected_only: bool) -> Vec<Cand> {
    let exe = {
        let installed = config::bin_dir().join(if cfg!(windows) { "reman.exe" } else { "reman" });
        if installed.exists() { installed } else { std::env::current_exe().unwrap_or_default() }
    };
    let mut v = vec![cand("all", if connected_only { "every connected agent" } else { "every installed agent" })];
    let http_on = settings::load().http.is_some();
    if !connected_only || http_on {
        v.push(cand("http", if http_on { "HTTP endpoint (on)" } else { "HTTP endpoint for SDKs (off)" }));
    }
    for (id, label) in connect::AGENTS {
        let state = match (connect::installed(id), connect::status(id, &exe)) {
            (_, Some(_)) => "connected",
            (true, None) => "installed, not connected",
            (false, None) => "not installed",
        };
        if connected_only && state != "connected" {
            continue;
        }
        v.push(cand(*id, format!("{label} - {state}")));
    }
    v
}

/// One request to a RUNNING daemon - completion must never start one (that's a ~1s stall).
fn ask(req: &Value) -> Option<Value> {
    let mut c = Client::connect_timeout(Duration::from_millis(150)).ok()?;
    c.call(req).ok()
}

fn history(cur: &str, status: Option<&str>) -> Vec<Cand> {
    let q = cur.trim_matches(['"', '\'']);
    let mut req = if q.is_empty() { json!({"op": "recent", "k": 40}) } else { json!({"op": "search", "query": q, "k": 40, "group": false}) };
    if let Some(s) = status {
        req["status"] = json!(s);
    }
    let Some(r) = ask(&req) else { return vec![] };
    r["results"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| {
                    let c = x["command"].as_str()?;
                    (!c.contains('\n')).then(|| cand(c, format!("{}x · {}", x["runs"], x["last_run"].as_str().unwrap_or(""))))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Folders with history outside every shared root, top-most first (subfolders fold in).
pub fn unshared_folders(roots: &[String]) -> Vec<(String, u64)> {
    let Some(r) = ask(&json!({"op": "folders", "k": 400})) else { return vec![] };
    let sep = std::path::MAIN_SEPARATOR;
    let inside = |p: &str, root: &str| {
        let (p, root) = (config::norm_path(p), config::norm_path(root));
        p == root || p.starts_with(&format!("{root}{sep}"))
    };
    let home = dirs::home_dir().map(|h| config::norm_path(&h.to_string_lossy())).unwrap_or_default();
    let mut rows: Vec<(String, u64)> = r["results"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| Some((x["cwd"].as_str()?.to_string(), x["runs"].as_u64().unwrap_or(0)))).collect())
        .unwrap_or_default();
    rows.retain(|(c, _)| {
        let n = config::norm_path(c);
        !roots.iter().any(|root| inside(c, root)) && !n.contains("appdata") && !n.contains("/tmp") && n != home && n.len() > 3
    });
    rows.sort_by_key(|(c, _)| c.len());
    let mut out: Vec<(String, u64)> = Vec::new();
    for (c, n) in rows {
        match out.iter_mut().find(|(p, _)| inside(&c, p)) {
            Some(parent) => parent.1 += n,
            None => out.push((c, n)),
        }
    }
    out.sort_by(|a, b| b.1.cmp(&a.1));
    out
}

/// Directories matching a partial path (`D:\Proj` -> `D:\Projects\`).
pub(crate) fn dirs(cur: &str) -> Vec<Cand> {
    let cur = cur.trim_matches(['"', '\'']);
    let (base, prefix) = match cur.rfind(['/', '\\']) {
        Some(i) => (&cur[..=i], &cur[i + 1..]),
        None => ("", cur),
    };
    let dir = if base.is_empty() { std::path::PathBuf::from(".") } else { std::path::PathBuf::from(base) };
    let Ok(rd) = std::fs::read_dir(&dir) else { return vec![] };
    let sep = if base.contains('/') { "/" } else { std::path::MAIN_SEPARATOR_STR };
    let mut v: Vec<Cand> = rd
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            (n.to_lowercase().starts_with(&prefix.to_lowercase()) && !n.starts_with('.')).then(|| cand(format!("{base}{n}{sep}"), "folder"))
        })
        .collect();
    v.sort_by(|a, b| a.value.cmp(&b.value));
    v.truncate(60);
    v
}

/// Words of the text before the cursor, minus the program name. Quotes group (`'git push'`),
/// backslashes stay literal (Windows paths). Shells pass the raw line so none of them has to
/// split it (bash would break `D:\x` at the colon, PowerShell 5.1 drops empty arguments).
pub fn split_line(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut any = false;
    for c in line.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                any = true;
            }
            (None, c) if c.is_whitespace() => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
                any = false;
            }
            (None, c) => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    out.into_iter().skip(1).collect()
}

/// Print for the shell: `value<TAB>help` (fish and our other glue all read this form).
pub fn print(root: &Command, done: &[String], cur: &str) {
    for c in candidates(root, done, cur) {
        let help = c.help.replace(['\t', '\n'], " ");
        println!("{}\t{}", c.value.replace(['\t', '\n'], " "), help);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{Arg, ArgAction};

    fn cli() -> Command {
        Command::new("reman")
            .subcommand(Command::new("connect").about("plug in agents").arg(Arg::new("targets").num_args(0..)).arg(Arg::new("port").long("port")).arg(Arg::new("print").long("print").action(ArgAction::SetTrue)).arg(Arg::new("old").long("old-history").value_parser(["on", "off"])))
            .subcommand(Command::new("init").about("shell integration").arg(Arg::new("shell")))
            .subcommand(Command::new("search").about("search"))
    }

    fn vals(done: &[&str], cur: &str) -> Vec<String> {
        let d: Vec<String> = done.iter().map(|s| s.to_string()).collect();
        candidates(&cli(), &d, cur).into_iter().map(|c| c.value).collect()
    }

    #[test]
    fn walks_subcommands_flags_and_values() {
        assert_eq!(vals(&[], "co"), ["connect"]);
        assert_eq!(vals(&[], "").len(), 3);
        assert_eq!(vals(&["init"], "b"), ["bash"]);
        assert!(vals(&["connect"], "--p").iter().any(|v| v == "--port") && vals(&["connect"], "--p").iter().any(|v| v == "--print"));
        assert_eq!(vals(&["connect", "--old-history"], ""), ["on", "off"]);
        // a flag's value is consumed, so the next word is positional again
        assert!(vals(&["connect", "--port", "8777"], "").iter().any(|v| v == "all"));
        assert!(vals(&["search"], "").contains(&"--help".to_string()));
    }

    #[test]
    fn splits_lines_like_a_shell() {
        assert_eq!(split_line(r#"reman forget 'git push --force' "#), ["forget", "git push --force"]);
        assert_eq!(split_line(r"reman.exe connect --add-root D:\Projects "), ["connect", "--add-root", r"D:\Projects"]);
        assert!(split_line("reman ").is_empty());
    }
}
