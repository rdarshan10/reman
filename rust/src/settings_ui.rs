//! `reman settings` - one page for everything onboarding set up, so nothing needs a command:
//! which coding tools reman is plugged into, which folders agents may see, the privacy switches,
//! the HTTP endpoint, and whether each shell is wired. Every change is written as it's made.
//! Drawn with the finder's renderer (whole-line repaints, named ANSI colours).
use crate::tui::{self, ACCENT, BAD, INFO, MUTED, OK, WARN, Screen};
use crate::{client, complete, connect, settings};
use anyhow::Result;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use ratatui::crossterm::{cursor, execute};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget, Wrap};
use unicode_width::UnicodeWidthStr;
use serde_json::json;
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone)]
enum Row {
    Header(&'static str),
    Note(String),
    Agent { id: &'static str, name: &'static str, installed: bool, connected: bool },
    Http { port: Option<u16> },
    Folder { path: String, shared: bool, runs: Option<u64> },
    AddFolder,
    /// only reman's own dialog can let an agent see a folder
    StrictPermissions(bool),
    Strict(bool),
    OldHistory(bool),
    /// "mask" | "drop" | "keep"
    Secrets(&'static str),
    Ai(bool),
    /// a notification when a long command of yours finishes: after this many seconds, or never
    DoneAlert(Option<u64>),
    /// the finder: lines under the prompt, or None for the whole screen
    FinderHeight(Option<u16>),
    FinderVim(bool),
    /// the finder lists agents' commands from the start
    FinderEveryone(bool),
    /// how many runs' output is kept (0 = none)
    OutputRuns(u32),
    /// shells open inside `reman shell`
    ShellLayer(bool),
    /// the keys' preset (standard, gentle, vim)
    KeyPreset(String),
    /// one action's keys: its id, its keys as shown ("" = off), whether the user changed it
    Key { id: &'static str, keys: String, changed: bool },
    Shell { name: &'static str, wired: bool, detail: String },
    Info { label: &'static str, value: String },
}

impl Row {
    fn selectable(&self) -> bool {
        !matches!(self, Row::Header(_) | Row::Note(_) | Row::Info { .. })
    }

    /// One sentence under the list saying what Enter does here.
    fn help(&self) -> String {
        match self {
            Row::Agent { installed: false, name, .. } => format!("{name} isn't installed on this machine. Install it, then connect it here."),
            Row::Agent { connected: true, name, .. } => format!("↵ disconnects {name}: removes reman from its config (a .reman-bak backup is kept)."),
            Row::Agent { name, .. } => format!("↵ connects {name}: reman's MCP server goes into its config, so it can search your real commands."),
            Row::Http { port: Some(_) } => "↵ turns the HTTP endpoint off. SDKs and scripts using it are refused at once.".into(),
            Row::Http { port: None } => "↵ turns on a local HTTP endpoint for SDKs (OpenAI Agents SDK, LangChain, curl). Localhost only, token required.".into(),
            Row::Folder { shared: true, .. } => "↵ stops sharing this folder with agents.".into(),
            Row::Folder { .. } => "↵ lets agents see commands you ran in this folder (and the folders inside it).".into(),
            Row::AddFolder => "↵ (or a) types a folder path to share. Tab completes.".into(),
            Row::StrictPermissions(true) => "On: an agent asking to see a folder gets reman's own dialog box, and only your click there shares it. Your AI app's Allow buttons can't. ↵ turns it off.".into(),
            Row::StrictPermissions(false) => "Off: you approve an agent's request with your AI app's own buttons, for that session. ↵ turns on reman's own dialog box instead, for apps set to approve tool calls by themselves.".into(),
            Row::Strict(_) => "Strict mode withholds any command that still looks like it holds a secret after redaction.".into(),
            Row::OldHistory(_) => "Old, imported history has no folder. When on, agents also get its generic commands (no paths, quotes or hosts).".into(),
            Row::Secrets(_) => "↵ cycles: mask the value (`export API_TOKEN=***`) · drop the command · keep as typed, so your finder shows it whole. Agents always get secrets masked. `reman scrub` cleans older history.".into(),
            Row::DoneAlert(_) => "↵ cycles: after 1 minute · after 5 minutes · never. A desktop notification when a command of yours that ran that long finishes, with how long it took.".into(),
            Row::FinderHeight(_) => "↵ cycles: 20 lines under the prompt · 30 lines · the whole screen. Where the finder (↑, Tab, Ctrl+R) opens.".into(),
            Row::FinderVim(_) => "↵ switches the finder's keys: as in a text box, or vim (Esc for normal mode: j k move, dd forgets, i types, q closes).".into(),
            Row::OutputRuns(_) => "↵ cycles: the newest 3000 runs · 300 · none. What commands printed, kept for `reman output` and the finder's ^O (agents' as they run, yours inside reman shell).".into(),
            Row::ShellLayer(_) => "↵ switches it: new shells open inside reman shell (a terminal layer of reman's own), so what your commands print is kept too.".into(),
            Row::KeyPreset(p) => {
                let what = crate::keys::PRESETS.iter().find(|x| x.0 == p).map(|x| x.1).unwrap_or("");
                format!("↵ cycles: standard · gentle · vim (starting over from one drops your changes). Now: {what}.")
            }
            Row::Key { id, .. } => match crate::keys::action(id) {
                Some(a) if a.fixed.is_some() => format!("↵ turns it on or off. {}.", cap(a.what)),
                Some(a) => format!("↵ then press the new key · Del turns it off · r gives it the preset's key. {}.", cap(a.what)),
                None => String::new(),
            },
            Row::FinderEveryone(_) => "↵ switches what the finder lists at first: your own commands, or agents' too. F3 in the finder changes it for the moment.".into(),
            Row::Ai(_) => "A model only arranges and explains commands that really worked (runbooks), never invents one. Remote endpoints: set \"ai\" in config.json.".into(),
            Row::Shell { wired: true, name, .. } => format!("{name} is wired. To remove it, delete the reman block from the file shown."),
            Row::Shell { name, .. } => format!("↵ wires {name} so its commands are recorded and the finder keys work."),
            _ => String::new(),
        }
    }
}

struct Page {
    rows: Vec<Row>,
    sel: usize,
    scroll: usize,
    message: Option<(String, Color)>,
    /// typing a folder to share: Some(text)
    input: Option<String>,
    /// changing an action's key: the next key pressed is the new one
    capture: Option<&'static str>,
}

fn exes() -> (PathBuf, PathBuf) {
    crate::agent_exes().unwrap_or_else(|_| {
        let me = std::env::current_exe().unwrap_or_default();
        let hook = crate::hook_exe(&me);
        (me, hook)
    })
}

fn build() -> Vec<Row> {
    let (exe, _) = exes();
    let st = settings::load();
    let mut r = vec![Row::Header("Coding tools")];
    for (id, name) in connect::AGENTS {
        r.push(Row::Agent { id, name, installed: connect::installed(id), connected: connect::status(id, &exe).is_some() });
    }
    r.push(Row::Http { port: st.http.as_ref().map(|h| h.port) });

    r.push(Row::Header("What agents can see"));
    if st.mcp_roots.is_empty() {
        r.push(Row::Note("Now: agents see no history. Share a folder to approve it for every agent.".into()));
    }
    for root in &st.mcp_roots {
        r.push(Row::Folder { path: root.clone(), shared: true, runs: None });
    }
    for (f, n) in complete::unshared_folders(&st.mcp_roots).into_iter().take(6) {
        r.push(Row::Folder { path: f, shared: false, runs: Some(n) });
    }
    r.push(Row::AddFolder);
    r.push(Row::StrictPermissions(st.strict_permissions));

    r.push(Row::Header("Privacy"));
    r.push(Row::Strict(st.strict_secrets));
    r.push(Row::OldHistory(st.share_old_history));
    r.push(Row::Secrets(st.secrets_mode()));
    let (nc, nf) = (st.ignore_commands.len(), st.ignore_folders.len());
    r.push(Row::Info { label: "never kept", value: if nc + nf == 0 { "commands typed with a leading space; add patterns as ignore_commands / ignore_folders in config.json".into() } else { format!("a leading space, {nc} command pattern(s), {nf} folder pattern(s) (config.json)") } });

    r.push(Row::Header("Alerts"));
    r.push(Row::DoneAlert(st.done_alert_after()));

    r.push(Row::Header("Finder"));
    r.push(Row::FinderHeight(st.finder_lines()));
    r.push(Row::FinderEveryone(st.finder_everyone()));
    r.push(Row::FinderVim(st.finder_vim()));

    r.push(Row::Header("What commands print"));
    r.push(Row::OutputRuns(st.output_runs()));
    r.push(Row::ShellLayer(st.shell_layer));

    r.push(Row::Header("Keys"));
    let map = crate::keys::Map::of(st.key_preset.as_deref(), &st.keys);
    r.push(Row::KeyPreset(map.preset.clone()));
    for (layer, title) in [(crate::keys::Layer::Shell, "in your shell (new terminals; open PowerShell windows at their next prompt)"), (crate::keys::Layer::Finder, "in the finder")] {
        r.push(Row::Note(title.into()));
        for a in crate::keys::ACTIONS.iter().filter(|a| a.layer == layer) {
            let keys = map.get(a.id).iter().map(crate::keys::Key::label).collect::<Vec<_>>().join(", ");
            r.push(Row::Key { id: a.id, keys, changed: st.keys.contains_key(a.id) });
        }
    }

    r.push(Row::Header("Language model (optional)"));
    r.push(Row::Ai(st.ai.as_ref().is_none_or(|a| a.enabled)));
    let endpoint = match st.ai.as_ref().and_then(|a| a.endpoint.clone()) {
        Some(e) => format!("{e}{}", st.ai.as_ref().and_then(|a| a.model.clone()).map(|m| format!("  (model {m})")).unwrap_or_default()),
        None => "auto: a model running on this machine (Ollama, LM Studio, llama.cpp), if any".into(),
    };
    r.push(Row::Info { label: "endpoint", value: endpoint });

    r.push(Row::Header("Shells"));
    if cfg!(windows) {
        let ps = crate::ps_wired();
        // wired but blocked (execution policy) is as good as not wired: Enter fixes it
        let blocked: Vec<_> = crate::ps_policy_blocks().into_iter().filter(|b| !b.2).collect();
        let detail = match (&ps, blocked.first()) {
            (Some(_), Some((exe, policy, _))) => format!("wired, but {} blocks profile scripts ({policy}): Enter allows your own", crate::ps_name(exe)),
            (Some(p), None) => p.display().to_string(),
            (None, _) => "not wired".into(),
        };
        r.push(Row::Shell { name: "PowerShell", wired: ps.is_some() && blocked.is_empty(), detail });
        let auto = crate::cmd_autorun();
        let cmd = if auto.contains("cmd-macros.txt") {
            Some("r / rr macros (AutoRun)".to_string())
        } else if auto.to_lowercase().contains("clink") {
            Some("through Clink".to_string())
        } else {
            None
        };
        r.push(Row::Shell { name: "Command Prompt", wired: cmd.is_some(), detail: cmd.unwrap_or_else(|| "not wired".into()) });
    } else {
        let w = crate::unix_rc_wired();
        let shell = std::env::var("SHELL").unwrap_or_default();
        let name = if shell.ends_with("zsh") { "zsh" } else if shell.ends_with("fish") { "fish" } else { "bash" };
        r.push(Row::Shell { name, wired: w.is_some(), detail: w.map(|(_, p)| p.display().to_string()).unwrap_or_else(|| "not wired".into()) });
    }

    r.push(Row::Header("reman"));
    let daemon = client::Client::connect_timeout(Duration::from_millis(200)).ok().and_then(|mut c| c.call(&json!({"op": "ping"})).ok());
    r.push(Row::Info {
        label: "daemon",
        value: match daemon {
            Some(p) => format!("running · pid {} · {} commands", p["pid"], p["indexed"]),
            None => "not running (starts by itself when needed)".into(),
        },
    });
    r.push(Row::Info { label: "data", value: crate::config::home().display().to_string() });
    r.push(Row::Info { label: "settings", value: settings::path().display().to_string() });
    r.push(Row::Info { label: "version", value: crate::config::VERSION.to_string() });
    r
}

/// Do what Enter means on this row. Returns the message to show.
fn act(row: &Row) -> Result<(String, Color)> {
    let (exe, hook) = exes();
    Ok(match row {
        Row::Agent { installed: false, name, .. } => (format!("{name} isn't installed here"), WARN),
        Row::Agent { id, name, connected: true, .. } => {
            connect::disconnect(id)?;
            (format!("disconnected {name}"), OK)
        }
        Row::Agent { id, name, .. } => {
            connect::connect(id, &exe, &hook)?;
            (format!("connected {name} - restart it to pick reman up"), OK)
        }
        Row::Http { port: Some(_) } => {
            crate::disable_http(&mut settings::load())?;
            ("HTTP endpoint off".into(), OK)
        }
        Row::Http { port: None } => {
            let port = crate::enable_http(&mut settings::load(), None)?;
            (format!("HTTP endpoint on: http://127.0.0.1:{port}/mcp (token in {})", settings::path().display()), OK)
        }
        Row::Folder { path, shared, .. } => {
            let mut st = settings::load();
            if *shared {
                st.mcp_roots.retain(|r| crate::config::norm_path(r) != crate::config::norm_path(path));
                settings::save(&st)?;
                (format!("agents no longer see {path}"), OK)
            } else {
                share(&mut st, path)?
            }
        }
        // turning it off lets an app's own approval share history: the user's call, never an agent's
        Row::StrictPermissions(true) if connect::run_by_agent().is_some() => {
            (connect::refuse_if_agent("Turning off strict permissions").unwrap_err().to_string(), BAD)
        }
        Row::StrictPermissions(on) => {
            let mut st = settings::load();
            st.strict_permissions = !on;
            settings::save(&st)?;
            if st.strict_permissions {
                ("strict permissions on: only reman's own dialog box can let an agent see a folder".into(), OK)
            } else {
                ("strict permissions off: your AI app's buttons approve, for that session".into(), WARN)
            }
        }
        Row::Strict(on) => {
            let mut st = settings::load();
            st.strict_secrets = !on;
            settings::save(&st)?;
            (format!("strict secrets {}", if st.strict_secrets { "on" } else { "off" }), OK)
        }
        Row::OldHistory(false) if connect::run_by_agent().is_some() => {
            (connect::refuse_if_agent("Sharing old history with agents").unwrap_err().to_string(), BAD)
        }
        Row::OldHistory(on) => {
            let mut st = settings::load();
            st.share_old_history = !on;
            settings::save(&st)?;
            (format!("old history {}", if st.share_old_history { "shared (generic commands only)" } else { "hidden from agents" }), OK)
        }
        Row::Secrets("drop") if connect::run_by_agent().is_some() => {
            (connect::refuse_if_agent("Turning off secret redaction").unwrap_err().to_string(), BAD)
        }
        Row::Secrets(mode) => {
            let mut st = settings::load();
            let (next, msg, c) = match *mode {
                "mask" => ("drop", "commands holding a secret are no longer recorded", OK),
                "drop" => ("keep", "secrets are kept as typed for you; agents still get them masked", WARN),
                _ => ("mask", "secrets are masked: the command is kept, the value hidden", OK),
            };
            st.secrets = (next != "mask").then(|| next.to_string());
            settings::save(&st)?;
            (msg.to_string(), c)
        }
        Row::DoneAlert(after) => {
            let mut st = settings::load();
            let (next, msg) = match after {
                Some(60) => (300, "done alerts after 5 minutes"),
                Some(_) => (0, "no done alerts"),
                None => (60, "done alerts after 1 minute"),
            };
            st.done_alert_after_s = (next != 60).then_some(next);
            settings::save(&st)?;
            (msg.to_string(), OK)
        }
        Row::FinderHeight(lines) => {
            let mut st = settings::load();
            let (next, msg) = match lines {
                Some(20) => (Some(30), "the finder takes 30 lines under the prompt"),
                Some(_) => (Some(0), "the finder takes the whole screen"),
                None => (None, "the finder takes 20 lines under the prompt"),
            };
            st.finder_height = next;
            settings::save(&st)?;
            (msg.to_string(), OK)
        }
        Row::FinderVim(on) => {
            let mut st = settings::load();
            st.finder_keys = (!on).then(|| "vim".to_string());
            settings::save(&st)?;
            (if *on { "the finder's keys work as in a text box" } else { "vim keys in the finder: Esc for normal mode" }.to_string(), OK)
        }
        Row::OutputRuns(n) => {
            let mut st = settings::load();
            let (next, msg) = match n {
                3000 => (300, "output kept for the newest 300 runs"),
                0 => (3000, "output kept for the newest 3000 runs"),
                _ => (0, "no output kept"),
            };
            st.output_runs = (next != 3000).then_some(next);
            settings::save(&st)?;
            (msg.to_string(), OK)
        }
        Row::ShellLayer(on) => {
            let mut st = settings::load();
            st.shell_layer = !on;
            settings::save(&st)?;
            (if *on { "new shells open as before" } else { "new shells open inside reman shell - open a new terminal" }.to_string(), OK)
        }
        Row::KeyPreset(now) => {
            let mut st = settings::load();
            let i = crate::keys::PRESETS.iter().position(|x| x.0 == now).unwrap_or(0);
            let (next, what) = crate::keys::PRESETS[(i + 1) % crate::keys::PRESETS.len()];
            st.key_preset = (next != "standard").then(|| next.to_string());
            st.finder_keys = (next == "vim").then(|| "vim".to_string());
            st.keys.clear();
            settings::save(&st)?;
            (format!("{next}: {what}"), OK)
        }
        Row::Key { id, keys, .. } => {
            // only a fixed one gets here (the others wait for their new key)
            let mut st = settings::load();
            let a = crate::keys::action(id).filter(|a| a.fixed.is_some()).ok_or_else(|| anyhow::anyhow!("press a key"))?;
            let on = !keys.is_empty();
            st.keys.insert(id.to_string(), if on { vec![] } else { vec![a.fixed.unwrap_or("").to_string()] });
            settings::save(&st)?;
            (format!("{} {}", cap(a.what), if on { "is off" } else { "is on" }), OK)
        }
        Row::FinderEveryone(on) => {
            let mut st = settings::load();
            st.finder_who = (!on).then(|| "all".to_string());
            settings::save(&st)?;
            (if *on { "the finder lists your own commands (F3 shows agents')" } else { "the finder lists agents' commands too" }.to_string(), OK)
        }
        Row::Ai(on) => {
            let mut st = settings::load();
            let mut ai = st.ai.clone().unwrap_or(settings::Ai { enabled: true, endpoint: None, model: None, api_key_env: None });
            ai.enabled = !on;
            st.ai = Some(ai);
            settings::save(&st)?;
            (format!("language model {}", if !on { "on: runbooks are arranged by a model when one is available" } else { "off: runbooks come from your history alone" }), OK)
        }
        Row::Shell { wired: true, detail, .. } => (format!("already wired: {detail}"), MUTED),
        Row::Shell { name, .. } => {
            let msg = match *name {
                "PowerShell" => {
                    let mut done = crate::wire_ps_profiles(&exe)?;
                    for (sh, _, locked) in crate::ps_policy_blocks() {
                        if !locked {
                            crate::allow_ps_scripts(sh)?;
                            done.push(format!("{} now runs your own scripts (RemoteSigned, your user only)", crate::ps_name(sh)));
                        }
                    }
                    done.join("; ")
                }
                "Command Prompt" => format!("wired r / rr via {}", crate::wire_cmd_macros(&exe)?.display()),
                _ => {
                    let (_, rc, _) = crate::wire_unix_rc(&exe)?;
                    format!("wired {}", rc.display())
                }
            };
            (format!("{msg} - open a new terminal"), OK)
        }
        Row::AddFolder | Row::Header(_) | Row::Note(_) | Row::Info { .. } => (String::new(), MUTED),
    })
}

/// The first letter up: `open the finder` -> `Open the finder`.
fn cap(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

/// A new key for an action, if it may be one (what's wrong with it otherwise, or what to know).
fn set_key(id: &'static str, key: &crate::keys::Key) -> Result<(String, Color)> {
    let mut st = settings::load();
    let map = crate::keys::Map::of(st.key_preset.as_deref(), &st.keys);
    let note = match crate::keys::check(&map, id, key) {
        Some((true, why)) => return Ok((format!("not {}: {why}", key.label()), BAD)),
        Some((false, why)) => Some(why),
        None => None,
    };
    st.keys.insert(id.to_string(), vec![key.label()]);
    settings::save(&st)?;
    Ok(match note {
        Some(why) => (format!("{} now; note: {why}", key.label()), WARN),
        None => (format!("{} now", key.label()), OK),
    })
}

fn share(st: &mut settings::Settings, path: &str) -> Result<(String, Color)> {
    if let Err(e) = connect::refuse_if_agent("Sharing a folder with agents") {
        return Ok((e.to_string(), BAD));
    }
    let abs = std::path::absolute(path.trim()).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| path.trim().to_string());
    if !std::path::Path::new(&abs).is_dir() {
        return Ok((format!("{abs} isn't a folder"), BAD));
    }
    if st.mcp_roots.iter().any(|r| crate::config::norm_path(r) == crate::config::norm_path(&abs)) {
        return Ok((format!("{abs} is already shared"), MUTED));
    }
    st.mcp_roots.push(abs.clone());
    settings::save(st)?;
    Ok((format!("agents may now see {abs}"), OK))
}

fn check(on: bool) -> Span<'static> {
    if on { Span::styled("[x] ", Style::default().fg(OK).add_modifier(Modifier::BOLD)) } else { Span::styled("[ ] ", Style::default().fg(MUTED)) }
}

fn row_line(row: &Row, sel: bool, w: usize) -> Line<'static> {
    let bar = Span::styled(if sel { " ▌ " } else { "   " }, Style::default().fg(ACCENT));
    let name_st = if sel { Style::default().add_modifier(Modifier::BOLD) } else { Style::default() };
    // names give up room first, so a state like "installed, not connected" stays whole
    let label_w = w.saturating_sub(31).clamp(16, 34);
    let label = |s: &str| format!("{} ", tui::fit(s, label_w.saturating_sub(1)));
    let state = |s: String, c: Color| Span::styled(tui::fit(&s, w.saturating_sub(7 + label_w)), Style::default().fg(c));
    match row {
        Row::Header(t) => Line::from(vec![Span::raw(" "), Span::styled(t.to_string(), Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))]),
        Row::Note(t) => Line::styled(format!("     {t}"), Style::default().fg(MUTED).add_modifier(Modifier::ITALIC)),
        Row::Agent { name, installed, connected, .. } => Line::from(vec![
            bar,
            check(*connected),
            Span::styled(label(name), if *installed { name_st } else { name_st.fg(MUTED) }),
            if *connected { state("connected".into(), OK) } else if *installed { state("installed, not connected".into(), WARN) } else { state("not installed".into(), MUTED) },
        ]),
        Row::Http { port } => Line::from(vec![
            bar,
            check(port.is_some()),
            Span::styled(label("HTTP endpoint (SDKs, scripts)"), name_st),
            match port {
                Some(p) => state(format!("on · http://127.0.0.1:{p}/mcp"), OK),
                None => state("off".into(), MUTED),
            },
        ]),
        Row::Folder { path, shared, runs } => Line::from(vec![
            bar,
            check(*shared),
            Span::styled(label(path), name_st),
            match (shared, runs) {
                (true, _) => state("shared with agents".into(), OK),
                (false, Some(n)) => state(format!("{n} runs, not shared"), MUTED),
                (false, None) => state("not shared".into(), MUTED),
            },
        ]),
        Row::AddFolder => Line::from(vec![bar, Span::styled("  + share another folder…", name_st.fg(ACCENT))]),
        Row::StrictPermissions(on) => Line::from(vec![bar, check(*on), Span::styled("Strict permissions: only reman's own dialog box can approve an agent", name_st)]),
        Row::Strict(on) => Line::from(vec![bar, check(*on), Span::styled("Strict secrets: withhold anything that still looks secret", name_st)]),
        Row::OldHistory(on) => Line::from(vec![bar, check(*on), Span::styled("Share generic commands from old, folder-less history", name_st)]),
        Row::Secrets(mode) => Line::from(vec![
            bar,
            Span::styled("Secrets in commands: ", name_st),
            match *mode {
                "drop" => state("drop the command".into(), OK),
                "keep" => state("keep as typed (agents still get them masked)".into(), WARN),
                _ => state("mask the value".into(), OK),
            },
        ]),
        Row::Ai(on) => Line::from(vec![bar, check(*on), Span::styled("Use a language model to arrange runbooks", name_st)]),
        Row::DoneAlert(after) => Line::from(vec![
            bar,
            Span::styled("Done alerts: ", name_st),
            match after {
                Some(60) => state("after 1 minute".into(), OK),
                Some(n) if n % 60 == 0 => state(format!("after {} minutes", n / 60), OK),
                Some(n) => state(format!("after {n} seconds"), OK),
                None => state("never".into(), MUTED),
            },
        ]),
        Row::FinderHeight(lines) => Line::from(vec![
            bar,
            Span::styled("Opens in: ", name_st),
            match lines {
                Some(n) => state(format!("{n} lines under the prompt"), OK),
                None => state("the whole screen".into(), OK),
            },
        ]),
        Row::FinderEveryone(on) => Line::from(vec![bar, check(*on), Span::styled("List agents' commands too", name_st)]),
        Row::KeyPreset(p) => Line::from(vec![bar, Span::styled("Preset: ", name_st), state(p.clone(), OK)]),
        Row::Key { id, keys, changed } => {
            let what = crate::keys::action(id).map(|a| cap(a.what)).unwrap_or_default();
            let shown = if keys.is_empty() { state("off".into(), MUTED) } else { state(format!("{keys}{}", if *changed { "  (yours)" } else { "" }), if *changed { WARN } else { OK }) };
            Line::from(vec![bar, Span::styled(label(&what), name_st), shown])
        }
        Row::OutputRuns(n) => Line::from(vec![
            bar,
            Span::styled("Kept for: ", name_st),
            if *n == 0 { state("nothing kept".into(), MUTED) } else { state(format!("the newest {n} runs"), OK) },
        ]),
        Row::ShellLayer(on) => Line::from(vec![bar, check(*on), Span::styled("Open shells inside reman shell (keeps what yours print)", name_st)]),
        Row::FinderVim(on) => Line::from(vec![bar, check(*on), Span::styled("Vim keys", name_st)]),
        Row::Shell { name, wired, detail } => Line::from(vec![
            bar,
            check(*wired),
            Span::styled(label(name), name_st),
            state(detail.clone(), if *wired { OK } else { WARN }),
        ]),
        Row::Info { label, value } => Line::from(vec![Span::raw("       "), Span::styled(format!("{label:<10}"), Style::default().fg(MUTED)), Span::raw(value.clone())]),
    }
}

fn draw(buf: &mut Buffer, p: &mut Page) -> (u16, u16) {
    let area = buf.area;
    let (w, h) = (area.width as usize, area.height as usize);
    // title: the logo with "settings" beside its last row (one plain line on a short terminal),
    // then a blank row
    let subtitle = |lead: &'static str, room: usize| {
        let tag = "   every change is saved as you make it";
        let mut l = vec![Span::styled(lead, Style::default().add_modifier(Modifier::BOLD))];
        if lead.chars().count() + tag.len() <= room {
            l.push(Span::styled(tag, Style::default().fg(MUTED)));
        }
        Line::from(l)
    };
    let top = if h >= 18 && w >= tui::LOGO_W as usize + 50 {
        for (i, line) in tui::logo_lines().into_iter().enumerate() {
            buf.set_line(1, i as u16, &line, tui::LOGO_W);
        }
        let x = tui::LOGO_W + 3;
        buf.set_line(x, 2, &subtitle("settings", (area.width - x) as usize), area.width - x);
        4
    } else {
        buf.set_line(0, 0, &subtitle(" reman settings", w), area.width);
        2
    };
    // list between the title and the 3-line footer
    let list_h = h.saturating_sub(top + 3);
    let lines: Vec<Line> = p.rows.iter().enumerate().map(|(i, r)| row_line(r, i == p.sel, w)).collect();
    // headers get a blank line above them
    let mut shown: Vec<(Line, usize)> = Vec::new();
    for (i, l) in lines.into_iter().enumerate() {
        if matches!(p.rows[i], Row::Header(_)) && !shown.is_empty() {
            shown.push((Line::raw(""), usize::MAX));
        }
        shown.push((l, i));
    }
    let sel_at = shown.iter().position(|(_, i)| *i == p.sel).unwrap_or(0);
    // the lines that say more about the selected row (where the model comes from) come into view
    // with it, as far as the page allows
    let about = shown[sel_at + 1..].iter().take_while(|(_, i)| p.rows.get(*i).is_some_and(|r| matches!(r, Row::Info { .. } | Row::Note(_)))).count().min(2);
    let last = (sel_at + about).min(sel_at + list_h.saturating_sub(1));
    if sel_at < p.scroll {
        p.scroll = sel_at.saturating_sub(1);
    } else if last >= p.scroll + list_h {
        p.scroll = last + 1 - list_h;
    }
    for (y, (line, _)) in shown.into_iter().skip(p.scroll).take(list_h).enumerate() {
        buf.set_line(0, (top + y) as u16, &line, area.width);
    }
    // footer: what Enter does here / the last result / the keys
    let help = p.rows.get(p.sel).map(Row::help).unwrap_or_default();
    Paragraph::new(Line::styled(tui::fit(&format!(" {help}"), w), Style::default().fg(INFO))).render(Rect::new(0, (h - 3) as u16, area.width, 1), buf);
    let mut cursor = (0, 0);
    if let Some(text) = &p.input {
        let prompt = " folder to share › ";
        // a path longer than the line shows its end, where the typing is
        let room = w.saturating_sub(prompt.chars().count() + 1);
        let n = text.chars().count();
        let shown: String = if n > room { text.chars().skip(n - room).collect() } else { text.clone() };
        Paragraph::new(Line::from(vec![Span::styled(prompt, Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)), Span::raw(shown.clone())]))
            .render(Rect::new(0, (h - 2) as u16, area.width, 1), buf);
        cursor = ((prompt.chars().count() + UnicodeWidthStr::width(shown.as_str())).min(w - 1) as u16, (h - 2) as u16);
    } else if let Some((m, c)) = &p.message {
        Paragraph::new(Line::styled(tui::fit(&format!(" {m}"), w), Style::default().fg(*c))).render(Rect::new(0, (h - 2) as u16, area.width, 1), buf);
    }
    let keys = if p.input.is_some() {
        vec![("↵", "share", "share"), ("tab", "complete", "complete"), ("esc", "cancel", "cancel")]
    } else {
        vec![("↑↓", "move", "move"), ("↵ / space", "change", "change"), ("a", "share a folder", "share"), ("esc", "close", "close")]
    };
    let long: usize = keys.iter().map(|(k, what, _)| UnicodeWidthStr::width(*k) + what.len() + 4).sum::<usize>() + 1;
    let fits = long <= w;
    let mut sp = vec![Span::raw(" ")];
    for (k, what, short) in keys {
        let k = if fits { k.to_string() } else { k.replace(" / space", "") };
        sp.push(Span::styled(k, Style::default().add_modifier(Modifier::BOLD)));
        sp.push(Span::styled(if fits { format!(" {what}   ") } else { format!(" {short}  ") }, Style::default().fg(MUTED)));
    }
    Paragraph::new(Line::from(sp)).render(Rect::new(0, (h - 1) as u16, area.width, 1), buf);
    cursor
}

fn step(p: &mut Page, dir: isize) {
    let n = p.rows.len() as isize;
    let mut i = p.sel as isize;
    loop {
        i += dir;
        if i < 0 || i >= n {
            return;
        }
        if p.rows[i as usize].selectable() {
            p.sel = i as usize;
            return;
        }
    }
}

pub fn run() -> Result<()> {
    let _cp = tui::Utf8Console::enable();
    let mut out = tui::tty()?;
    enable_raw_mode()?;
    execute!(out, EnterAlternateScreen, cursor::Show)?;
    let _ = out.write_all(b"\x1b[?7l");
    let mut screen = Screen { out: std::io::BufWriter::with_capacity(1 << 16, tui::tty()?), prev: Vec::new(), origin: 0 };
    screen.invalidate();
    let r = page_loop(&mut screen);
    disable_raw_mode()?;
    let _ = out.write_all(b"\x1b[?7h");
    execute!(out, LeaveAlternateScreen, cursor::Show)?;
    r
}

fn page_loop(screen: &mut Screen) -> Result<()> {
    let mut p = Page { rows: build(), sel: 0, scroll: 0, message: None, input: None, capture: None };
    step(&mut p, 1);
    let mut size = terminal::size().unwrap_or((80, 24));
    loop {
        let mut buf = Buffer::empty(Rect::new(0, 0, size.0, size.1));
        if size.0 < 40 || size.1 < 12 {
            Paragraph::new("reman settings: make the terminal a little bigger").wrap(Wrap { trim: true }).render(buf.area, &mut buf);
            screen.frame(&buf, (0, 0))?;
        } else {
            let cur = draw(&mut buf, &mut p);
            screen.frame(&buf, cur)?;
        }
        let ev = event::read()?;
        let k = match ev {
            Event::Key(k) if k.kind == KeyEventKind::Press => k,
            Event::Resize(w, h) => {
                size = (w, h);
                screen.invalidate();
                continue;
            }
            _ => continue,
        };
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        // typing a folder path
        if let Some(text) = p.input.as_mut() {
            match k.code {
                KeyCode::Esc => p.input = None,
                KeyCode::Enter => {
                    let path = std::mem::take(text);
                    p.input = None;
                    p.message = Some(share(&mut settings::load(), &path).unwrap_or_else(|e| (format!("{e:#}"), BAD)));
                    p.rows = build();
                }
                KeyCode::Tab => {
                    if let Some(c) = complete::dirs(text).first() {
                        *text = c.value.clone();
                    }
                }
                KeyCode::Backspace => {
                    text.pop();
                }
                KeyCode::Char('u') if ctrl => text.clear(),
                KeyCode::Char(c) if !ctrl => text.push(c),
                _ => {}
            }
            continue;
        }
        if let Some(id) = p.capture.take() {
            p.message = Some(match (k.code, crate::keys::Key::from_event(&k)) {
                (KeyCode::Esc, _) => ("unchanged".into(), MUTED),
                (_, None) => ("that key can't be one of reman's".into(), BAD),
                (_, Some(key)) => set_key(id, &key).unwrap_or_else(|e| (format!("{e:#}"), BAD)),
            });
            let was = p.sel;
            p.rows = build();
            p.sel = was.min(p.rows.len().saturating_sub(1));
            continue;
        }
        if let Row::Key { id, .. } = p.rows[p.sel].clone() {
            let fixed = crate::keys::action(id).is_some_and(|a| a.fixed.is_some());
            let done = match k.code {
                KeyCode::Enter | KeyCode::Char(' ') if !fixed => {
                    p.capture = Some(id);
                    p.message = Some((format!("press the new key for: {} (Esc leaves it)", crate::keys::action(id).map(|a| a.what).unwrap_or(id)), ACCENT));
                    true
                }
                KeyCode::Delete | KeyCode::Char('r') => {
                    let mut st = settings::load();
                    if k.code == KeyCode::Delete {
                        st.keys.insert(id.to_string(), vec![]);
                    } else {
                        st.keys.remove(id);
                    }
                    p.message = Some(match settings::save(&st) {
                        Ok(_) if k.code == KeyCode::Delete => ("off: that key does what it did before reman".into(), OK),
                        Ok(_) => ("back to the preset's key".into(), OK),
                        Err(e) => (format!("{e:#}"), BAD),
                    });
                    let was = p.sel;
                    p.rows = build();
                    p.sel = was;
                    true
                }
                _ => false,
            };
            if done {
                continue;
            }
        }
        match k.code {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(()),
            KeyCode::Char('c') if ctrl => return Ok(()),
            KeyCode::Up | KeyCode::Char('k') => step(&mut p, -1),
            KeyCode::Down | KeyCode::Char('j') => step(&mut p, 1),
            KeyCode::Home => {
                p.sel = 0;
                step(&mut p, 1);
            }
            KeyCode::End => {
                p.sel = p.rows.len();
                step(&mut p, -1);
            }
            KeyCode::Char('a') => p.input = Some(String::new()),
            KeyCode::Enter | KeyCode::Char(' ') => {
                let row = p.rows[p.sel].clone();
                if matches!(row, Row::AddFolder) {
                    p.input = Some(String::new());
                    continue;
                }
                // show that something is happening (the Claude CLI takes a moment)
                p.message = Some(("working…".into(), MUTED));
                let mut buf = Buffer::empty(Rect::new(0, 0, size.0, size.1));
                let cur = draw(&mut buf, &mut p);
                screen.frame(&buf, cur)?;
                p.message = Some(act(&row).unwrap_or_else(|e| (format!("{e:#}"), BAD)));
                let keep = std::mem::discriminant(&row);
                let was = p.sel;
                p.rows = build();
                // stay on the same row (or the nearest selectable one)
                p.sel = was.min(p.rows.len().saturating_sub(1));
                if !p.rows[p.sel].selectable() || std::mem::discriminant(&p.rows[p.sel]) != keep {
                    step(&mut p, 1);
                }
            }
            _ => {}
        }
    }
}
