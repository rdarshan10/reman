//! Offline, no-execution command understanding: program/subcommand parsing, variant grouping,
//! tldr-pages descriptions and repo identity. Port of reman_enrich.py (parse_cmd, describe,
//! group_key, repo_identity) - nothing here ever runs a binary.
use parking_lot::Mutex;
use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

static TLDR_JSON: &[u8] = include_bytes!("../../tldr_map.json");

#[derive(serde::Deserialize)]
struct TldrMap {
    cmd: HashMap<String, String>,
    ex: HashMap<String, String>,
}

fn tldr() -> &'static TldrMap {
    static M: OnceLock<TldrMap> = OnceLock::new();
    M.get_or_init(|| serde_json::from_slice(TLDR_JSON).unwrap_or(TldrMap { cmd: HashMap::new(), ex: HashMap::new() }))
}

const NON_TOOLS: &[&str] = &[
    "#", "&", "cd", "if", "for", "while", "function", "async", "from", "import", "echo", "cls", "clear", "$", "{",
    "}", "(", ")", "set", "return",
];

fn alias(p: &str) -> &str {
    match p {
        "pip3" => "pip",
        "python3" | "python3.8" | "py" => "python",
        _ => p,
    }
}

fn is_drive_hop(t: &str) -> bool {
    // `d:` / `d:;` - PowerShell drive switch preceding the real command
    let b = t.as_bytes();
    (b.len() == 2 || (b.len() == 3 && b[2] == b';')) && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// (program, subcommand) with leading `cd x;`, `&`, drive hops etc. skipped.
pub fn parse_cmd(cmd: &str) -> (Option<String>, Option<String>) {
    let toks: Vec<&str> = cmd.split_whitespace().collect();
    let mut i = 0;
    while i < toks.len() {
        let t = toks[i];
        let tl = t.to_lowercase();
        if matches!(t, "&" | "&&" | "|" | ";") || is_drive_hop(&tl) || tl == "cd" || tl == "set" || t.ends_with(';') {
            i += 1;
            continue;
        }
        break;
    }
    if i >= toks.len() {
        return (None, None);
    }
    let raw = toks[i].trim_matches(|c| c == '"' || c == '\'').replace('\\', "/");
    let mut prog = raw.rsplit('/').next().unwrap_or("").to_lowercase();
    if let Some(p) = prog.strip_suffix(".exe") {
        prog = p.to_string();
    }
    let prog = alias(&prog).to_string();
    let mut sub = None;
    for t in &toks[i + 1..] {
        if t.starts_with('-') {
            break;
        }
        if !t.contains('/') && !t.contains('\\') && !t.contains('=') && !t.starts_with('"') {
            sub = Some(t.to_lowercase());
            break;
        }
    }
    (Some(prog), sub)
}

fn is_bare_word(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b[0].is_ascii_lowercase()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_' || *c == b'-')
}

/// Collapse variants under one intent: `git commit -m "x"` -> `git commit`, `scp a b` -> `scp`.
pub fn group_key(cmd: &str) -> String {
    match parse_cmd(cmd) {
        (Some(p), Some(s)) if is_bare_word(&s) => format!("{p} {s}"),
        (Some(p), _) => p,
        (None, _) => cmd.trim().chars().take(40).collect(),
    }
}

pub struct Description {
    pub display: Option<String>,
    /// ("gen"|"spec", text) - embedded separately and max-blended at query time
    pub parts: Vec<(&'static str, String)>,
}

pub fn describe(cmd: &str) -> Description {
    let (prog, sub) = parse_cmd(cmd);
    let Some(prog) = prog.filter(|p| !NON_TOOLS.contains(&p.as_str())) else {
        return Description { display: None, parts: vec![] };
    };
    let m = tldr();
    let mut parts = Vec::new();
    if let Some(g) = m.cmd.get(&prog) {
        parts.push(("gen", format!("{prog}: {g}")));
    }
    if let Some(sub) = sub {
        let mut spec: Vec<&String> = Vec::new();
        if let Some(g) = m.cmd.get(&format!("{prog}-{sub}")) {
            spec.push(g);
        }
        if let Some(g) = m.ex.get(&format!("{prog}\0{sub}")) {
            if !spec.iter().any(|s| s.eq_ignore_ascii_case(g)) {
                spec.push(g);
            }
        }
        if !spec.is_empty() {
            let joined: Vec<&str> = spec.iter().map(|s| s.as_str()).collect();
            parts.push(("spec", format!("{prog}: {}", joined.join("; "))));
        }
    }
    let display = parts.iter().find(|p| p.0 == "spec").or_else(|| parts.first()).map(|p| p.1.clone());
    Description { display, parts }
}

fn read_origin(cfg: &Path) -> Option<String> {
    let text = std::fs::read_to_string(cfg).ok()?;
    let mut sect: Option<String> = None;
    let mut urls: Vec<(String, String)> = Vec::new();
    for ln in text.lines() {
        let s = ln.trim();
        if s.starts_with('[') && s.ends_with(']') {
            sect = Some(s[1..s.len() - 1].trim().to_string());
        } else if let Some(sec) = &sect {
            if sec.starts_with("remote ") && s.to_lowercase().starts_with("url") && s.contains('=') {
                let name = sec.split('"').nth(1).unwrap_or(sec).to_string();
                urls.push((name, s.split_once('=').unwrap().1.trim().to_string()));
            }
        }
    }
    urls.iter().find(|(n, _)| n == "origin").or(urls.first()).map(|(_, u)| u.clone())
}

/// Repo a folder belongs to: git origin url (parsed from .git/config, never runs git), else the
/// repo root, else the folder itself. Cached - called once per distinct cwd.
pub fn repo_identity(path: &str) -> String {
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(v) = cache.lock().get(path) {
        return v.clone();
    }
    let start = Path::new(path);
    let mut d = Some(start);
    let mut ident = crate::config::norm_path(path);
    while let Some(dir) = d {
        let gitdir = dir.join(".git");
        let cfg = gitdir.join("config");
        if cfg.is_file() {
            ident = read_origin(&cfg).unwrap_or_else(|| crate::config::norm_path(&dir.to_string_lossy()));
            break;
        }
        if gitdir.is_dir() {
            ident = crate::config::norm_path(&dir.to_string_lossy());
            break;
        }
        d = dir.parent();
    }
    cache.lock().insert(path.to_string(), ident.clone());
    ident
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups() {
        assert_eq!(group_key(r#"git commit -m "x""#), "git commit");
        assert_eq!(group_key("alembic upgrade head"), "alembic upgrade");
        assert_eq!(group_key(r#"scp -r "app" root@h:/p"#), "scp");
        assert_eq!(group_key("scp index.html root@h:/p"), "scp");
        assert_eq!(group_key("cd x; npm run dev"), "npm run");
        assert_eq!(group_key(r"C:\Python\python.exe app.py"), "python");
    }

    #[test]
    fn describes_from_tldr() {
        let d = describe("git commit -m wip");
        assert!(d.display.unwrap().starts_with("git:"));
        assert!(d.parts.iter().any(|p| p.0 == "gen"));
        assert!(describe("# a comment").display.is_none());
    }
}
