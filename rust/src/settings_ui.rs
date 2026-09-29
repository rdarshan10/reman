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
use ratatui::widgets::{Paragraph, Widget};
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
    Strict(bool),
    OldHistory(bool),
    DropSecrets(bool),
    Ai(bool),
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
            Row::Strict(_) => "Strict mode withholds any command that still looks like it holds a secret after redaction.".into(),
            Row::OldHistory(_) => "Old, imported history has no folder. When on, agents also get its generic commands (no paths, quotes or hosts).".into(),
            Row::DropSecrets(_) => "A command holding a secret is recorded with the value masked (`export API_TOKEN=***`); on, it is not recorded at all. `reman scrub` cleans older history.".into(),
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
        r.push(Row::Note("Now: each agent sees only the project it's working in. Share a folder to let every agent see it.".into()));
    }
    for root in &st.mcp_roots {
        r.push(Row::Folder { path: root.clone(), shared: true, runs: None });
    }
    for (f, n) in complete::unshared_folders(&st.mcp_roots).into_iter().take(6) {
        r.push(Row::Folder { path: f, shared: false, runs: Some(n) });
    }
    r.push(Row::AddFolder);

    r.push(Row::Header("Privacy"));
    r.push(Row::Strict(st.strict_secrets));
    r.push(Row::OldHistory(st.share_old_history));
    r.push(Row::DropSecrets(st.drop_secrets()));
    let (nc, nf) = (st.ignore_commands.len(), st.ignore_folders.len());
    r.push(Row::Info { label: "never kept", value: if nc + nf == 0 { "commands typed with a leading space; add patterns as ignore_commands / ignore_folders in config.json".into() } else { format!("a leading space, {nc} command pattern(s), {nf} folder pattern(s) (config.json)") } });

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
        Row::Strict(on) => {
            let mut st = settings::load();
            st.strict_secrets = !on;
            settings::save(&st)?;
            (format!("strict secrets {}", if st.strict_secrets { "on" } else { "off" }), OK)
        }
        Row::OldHistory(on) => {
            let mut st = settings::load();
            st.share_old_history = !on;
            settings::save(&st)?;
            (format!("old history {}", if st.share_old_history { "shared (generic commands only)" } else { "hidden from agents" }), OK)
        }
        Row::DropSecrets(on) => {
            let mut st = settings::load();
            st.secrets = if *on { None } else { Some("drop".into()) };
            settings::save(&st)?;
            (if *on { "secrets are masked: the command is kept, the value hidden" } else { "commands holding a secret are no longer recorded" }.to_string(), OK)
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

fn share(st: &mut settings::Settings, path: &str) -> Result<(String, Color)> {
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
    let label_w = 34.min(w.saturating_sub(24));
    let state = |s: String, c: Color| Span::styled(s, Style::default().fg(c));
    match row {
        Row::Header(t) => Line::from(vec![Span::raw(" "), Span::styled(t.to_string(), Style::default().fg(ACCENT).add_modifier(Modifier::BOLD))]),
        Row::Note(t) => Line::styled(format!("     {t}"), Style::default().fg(MUTED).add_modifier(Modifier::ITALIC)),
        Row::Agent { name, installed, connected, .. } => Line::from(vec![
            bar,
            check(*connected),
            Span::styled(tui::fit(name, label_w), if *installed { name_st } else { name_st.fg(MUTED) }),
            if *connected { state("connected".into(), OK) } else if *installed { state("installed, not connected".into(), WARN) } else { state("not installed".into(), MUTED) },
        ]),
        Row::Http { port } => Line::from(vec![
            bar,
            check(port.is_some()),
            Span::styled(tui::fit("HTTP endpoint (SDKs, scripts)", label_w), name_st),
            match port {
                Some(p) => state(format!("on · http://127.0.0.1:{p}/mcp"), OK),
                None => state("off".into(), MUTED),
            },
        ]),
        Row::Folder { path, shared, runs } => Line::from(vec![
            bar,
            check(*shared),
            Span::styled(tui::fit(path, label_w), name_st),
            match (shared, runs) {
                (true, _) => state("shared with agents".into(), OK),
                (false, Some(n)) => state(format!("{n} runs, not shared"), MUTED),
                (false, None) => state("not shared".into(), MUTED),
            },
        ]),
        Row::AddFolder => Line::from(vec![bar, Span::styled("  + share another folder…", name_st.fg(ACCENT))]),
        Row::Strict(on) => Line::from(vec![bar, check(*on), Span::styled("Strict secrets: withhold anything that still looks secret", name_st)]),
        Row::OldHistory(on) => Line::from(vec![bar, check(*on), Span::styled("Share generic commands from old, folder-less history", name_st)]),
        Row::DropSecrets(on) => Line::from(vec![bar, check(*on), Span::styled("Drop commands holding a secret (off: mask the value)", name_st)]),
        Row::Ai(on) => Line::from(vec![bar, check(*on), Span::styled("Use a language model to arrange runbooks", name_st)]),
        Row::Shell { name, wired, detail } => Line::from(vec![
            bar,
            check(*wired),
            Span::styled(tui::fit(name, label_w), name_st),
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
    let subtitle = |lead: &'static str| {
        Line::from(vec![
            Span::styled(lead, Style::default().add_modifier(Modifier::BOLD)),
            Span::styled("   every change is saved as you make it", Style::default().fg(MUTED)),
        ])
    };
    let top = if h >= 18 && w >= tui::LOGO_W as usize + 50 {
        for (i, line) in tui::logo_lines().into_iter().enumerate() {
            buf.set_line(1, i as u16, &line, tui::LOGO_W);
        }
        let x = tui::LOGO_W + 3;
        buf.set_line(x, 2, &subtitle("settings"), area.width - x);
        4
    } else {
        buf.set_line(0, 0, &subtitle(" reman settings"), area.width);
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
    if sel_at < p.scroll {
        p.scroll = sel_at.saturating_sub(1);
    } else if sel_at >= p.scroll + list_h {
        p.scroll = sel_at + 1 - list_h;
    }
    for (y, (line, _)) in shown.into_iter().skip(p.scroll).take(list_h).enumerate() {
        buf.set_line(0, (top + y) as u16, &line, area.width);
    }
    // footer: what Enter does here / the last result / the keys
    let help = p.rows.get(p.sel).map(Row::help).unwrap_or_default();
    Paragraph::new(Line::styled(format!(" {help}"), Style::default().fg(INFO))).render(Rect::new(0, (h - 3) as u16, area.width, 1), buf);
    let mut cursor = (0, 0);
    if let Some(text) = &p.input {
        let prompt = " folder to share › ";
        Paragraph::new(Line::from(vec![Span::styled(prompt, Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)), Span::raw(text.clone())]))
            .render(Rect::new(0, (h - 2) as u16, area.width, 1), buf);
        cursor = ((prompt.chars().count() + text.chars().count()).min(w - 1) as u16, (h - 2) as u16);
    } else if let Some((m, c)) = &p.message {
        Paragraph::new(Line::styled(format!(" {m}"), Style::default().fg(*c))).render(Rect::new(0, (h - 2) as u16, area.width, 1), buf);
    }
    let keys = if p.input.is_some() {
        vec![("↵", "share"), ("tab", "complete"), ("esc", "cancel")]
    } else {
        vec![("↑↓", "move"), ("↵ / space", "change"), ("a", "share a folder"), ("esc", "close")]
    };
    let mut sp = vec![Span::raw(" ")];
    for (k, what) in keys {
        sp.push(Span::styled(k, Style::default().add_modifier(Modifier::BOLD)));
        sp.push(Span::styled(format!(" {what}   "), Style::default().fg(MUTED)));
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
    let mut screen = Screen { out: std::io::BufWriter::with_capacity(1 << 16, tui::tty()?), prev: Vec::new() };
    screen.invalidate();
    let r = page_loop(&mut screen);
    disable_raw_mode()?;
    let _ = out.write_all(b"\x1b[?7h");
    execute!(out, LeaveAlternateScreen, cursor::Show)?;
    r
}

fn page_loop(screen: &mut Screen) -> Result<()> {
    let mut p = Page { rows: build(), sel: 0, scroll: 0, message: None, input: None };
    step(&mut p, 1);
    let mut size = terminal::size().unwrap_or((80, 24));
    loop {
        let mut buf = Buffer::empty(Rect::new(0, 0, size.0, size.1));
        if size.0 < 40 || size.1 < 12 {
            Paragraph::new("reman settings: make the terminal a little bigger").render(buf.area, &mut buf);
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
