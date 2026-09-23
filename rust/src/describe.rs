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

/// A command that says nothing about the project it ran in: one line, a well-known tool as the
/// first word, and no paths, quotes, URLs, hosts, variables or assignments in the arguments.
/// `docker compose up -d` and `alembic upgrade head` are generic; `git commit -m "..."`,
/// `python D:\x\train.py` and `ssh me@host` are not.
pub fn is_generic(cmd: &str) -> bool {
    let t = cmd.trim();
    if t.is_empty() || t.len() > 100 || t.contains('\n') {
        return false;
    }
    if t.chars().any(|c| matches!(c, '/' | '\\' | '~' | '@' | '"' | '\'' | '`' | '$' | '=' | '%' | '<' | '>' | '#')) {
        return false;
    }
    let first = t.split_whitespace().next().unwrap_or("").to_lowercase();
    let first = first.strip_suffix(".exe").unwrap_or(&first);
    let prog = alias(first);
    // file names (train.py), hostnames and domains carry project detail too; versions don't
    if t.split_whitespace().skip(1).any(|a| a.contains('.') && !a.chars().all(|c| c.is_ascii_digit() || c == '.' || c == 'v')) {
        return false;
    }
    !NON_TOOLS.contains(&prog) && tldr().cmd.contains_key(prog)
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

/// Variants of ONE command differ only in their data: `git commit -m "a"` / `git commit -m "b"`,
/// `kill 4121` / `kill 977`, `code D:\x\a.py` / `code D:\y\b.py`. Quoted strings, paths, numbers
/// and hashes collapse; everything else (subcommands, flags, service names) stays, so
/// `docker compose exec web alembic upgrade head` never folds into `docker compose logs`.
pub fn shape_key(cmd: &str) -> String {
    let mut out = String::with_capacity(cmd.len());
    let mut chars = cmd.trim().chars().peekable();
    while let Some(c) = chars.next() {
        if c == '"' || c == '\'' {
            for d in chars.by_ref() {
                if d == c {
                    break;
                }
            }
            out.push_str("\"_\"");
        } else {
            out.push(c.to_ascii_lowercase());
        }
    }
    out.split_whitespace()
        .map(|t| {
            let digits = t.chars().filter(char::is_ascii_digit).count();
            if t.contains('/') || t.contains('\\') {
                "_path"
            } else if digits > 0 && t.chars().all(|c| c.is_ascii_hexdigit() || matches!(c, '.' | ':' | '-')) && (digits * 2 >= t.len() || t.len() >= 7) {
                "_n"
            } else {
                t
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub struct Description {
    pub display: Option<String>,
    /// ("gen"|"spec", text) - embedded separately and max-blended at query time
    pub parts: Vec<(&'static str, String)>,
}

/// The command a wrapper runs: `docker exec web alembic upgrade head` -> `alembic upgrade head`,
/// likewise `docker compose exec|run <svc>`, `kubectl exec <pod> --`, `sudo`, `npx`, `uv run`,
/// `poetry run`, `pipenv run`, `python -m <module>`. None when `cmd` isn't a known wrapper.
pub fn wrapped(cmd: &str) -> Option<String> {
    let toks: Vec<&str> = cmd.split_whitespace().collect();
    let low: Vec<String> = toks.iter().map(|t| t.to_lowercase()).collect();
    let p0 = low.first()?.trim_end_matches(".exe");
    // flags that take a value in the wrappers above
    const VALUED: &[&str] = &["-u", "--user", "-w", "--workdir", "-e", "--env", "--env-file", "-f", "--file", "-p", "--project-name", "-c", "--container", "-n", "--namespace", "--name"];
    let skip_flags = |mut i: usize| {
        while i < toks.len() && toks[i].starts_with('-') && toks[i] != "--" {
            i += if VALUED.contains(&low[i].as_str()) && !toks[i].contains('=') { 2 } else { 1 };
        }
        i
    };
    let start = match (p0, low.get(1).map(String::as_str), low.get(2).map(String::as_str)) {
        ("docker", Some("exec"), _) => Some(skip_flags(2) + 1),
        ("docker", Some("compose"), Some("exec" | "run")) => Some(skip_flags(skip_flags(3)) + 1),
        ("docker-compose", Some("exec" | "run"), _) => Some(skip_flags(2) + 1),
        ("docker", Some("compose"), _) | ("docker-compose", _, _) => {
            // docker compose -f x.yml exec web ...
            let i = skip_flags(if p0 == "docker" { 2 } else { 1 });
            matches!(low.get(i).map(String::as_str), Some("exec" | "run")).then(|| skip_flags(i + 1) + 1)
        }
        ("kubectl", Some("exec"), _) => low.iter().position(|t| t == "--").map(|i| i + 1),
        ("sudo" | "npx" | "pnpx" | "bunx", _, _) => Some(skip_flags(1)),
        ("uv" | "poetry" | "pipenv" | "pdm", Some("run"), _) => Some(skip_flags(2)),
        ("python" | "python3" | "py", Some("-m"), _) => Some(2),
        _ => None,
    }?;
    let inner = toks.get(start..)?.join(" ");
    (!inner.trim().is_empty()).then_some(inner)
}

pub fn describe(cmd: &str) -> Description {
    let mut d = describe_one(cmd);
    // the wrapped command says what it's FOR (`alembic upgrade head` = migrations), so it leads
    if let Some(inner) = wrapped(cmd) {
        let i = describe_one(&inner);
        if !i.parts.is_empty() {
            d.display = i.display.clone().or(d.display);
            // own kinds: command_desc_vec is keyed (command, kind)
            let mut parts: Vec<(&'static str, String)> = i.parts.into_iter().map(|(k, t)| (if k == "gen" { "wgen" } else { "wspec" }, t)).collect();
            parts.extend(d.parts);
            d.parts = parts;
        }
    }
    d
}

fn describe_one(cmd: &str) -> Description {
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

#[cfg(test)]
mod wrapper_tests {
    use super::{describe, wrapped};

    #[test]
    fn sees_through_wrappers() {
        assert_eq!(wrapped("docker exec planet_web alembic upgrade head").as_deref(), Some("alembic upgrade head"));
        assert_eq!(wrapped("docker exec -it -u postgres db psql -U x").as_deref(), Some("psql -U x"));
        assert_eq!(wrapped("docker compose exec web alembic upgrade head").as_deref(), Some("alembic upgrade head"));
        assert_eq!(wrapped("docker-compose run --rm web pytest -q").as_deref(), Some("pytest -q"));
        assert_eq!(wrapped("docker compose -f prod.yml exec web alembic current").as_deref(), Some("alembic current"));
        assert_eq!(wrapped("kubectl exec -it api-7f -- python manage.py migrate").as_deref(), Some("python manage.py migrate"));
        assert_eq!(wrapped("python -m pytest tests").as_deref(), Some("pytest tests"));
        assert_eq!(wrapped("docker compose up -d"), None);
        assert_eq!(wrapped("git status"), None);
        let d = describe("docker exec planet_web alembic upgrade head");
        assert!(d.parts.iter().any(|p| p.1.to_lowercase().contains("migration")), "{:?}", d.parts);
    }
}

#[cfg(test)]
mod shape_tests {
    use super::shape_key;

    #[test]
    fn folds_data_not_intent() {
        assert_eq!(shape_key(r#"git commit -m "fix a""#), shape_key("git commit -m 'other'"));
        assert_eq!(shape_key("kill 4121"), shape_key("kill 977"));
        assert_eq!(shape_key(r"code D:\x\a.py"), shape_key("code /d/y/b.py"));
        assert_eq!(shape_key("git show 3b3911a"), shape_key("git show a201b52"));
        assert_ne!(shape_key("docker compose exec web alembic upgrade head"), shape_key("docker compose logs -f web"));
        assert_ne!(shape_key("npm run dev"), shape_key("npm run build"));
    }
}

#[cfg(test)]
mod generic_tests {
    use super::is_generic;

    #[test]
    fn generic_only_when_project_neutral() {
        for c in ["docker ps", "docker-compose up -d", "git status", "pip install numpy", "npm run dev", "kubectl get pods"] {
            assert!(is_generic(c), "{c}");
        }
        for c in ["git commit -m \"fix login\"", r"python D:\x\train.py", "ssh me@host", "cd udemy", "curl https://x.io",
                  "export TOKEN=abc", "ping db.internal.corp", "python train.py","myscript --go", "return db_obj", "echo hi"] {
            assert!(!is_generic(c), "{c}");
        }
    }
}
