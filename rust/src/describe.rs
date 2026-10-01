//! Offline, no-execution command understanding: program/subcommand parsing, variant grouping,
//! tldr-pages descriptions. Port of reman_enrich.py (parse_cmd, describe, group_key) - nothing
//! here ever runs a binary. (Repo identity is in git.rs.)
use std::collections::HashMap;
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

/// Every program a command line runs: the first word of each `&&` / `;` / `|` step, and what a
/// wrapper runs inside it (`npx astro dev` runs astro). `cd x` steps run nothing.
pub fn programs(cmd: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for seg in cmd.split([';', '|', '&', '\n', '(', ')']) {
        let seg = seg.trim();
        let first = seg.split_whitespace().next().unwrap_or("").to_lowercase();
        if seg.is_empty() || matches!(first.as_str(), "cd" | "pushd" | "popd" | "set-location" | "sl") {
            continue;
        }
        for s in std::iter::once(seg.to_string()).chain(wrapped(seg)) {
            if let (Some(p), _) = parse_cmd(&s) {
                let name_like = p.len() >= 2 && p.starts_with(|c: char| c.is_ascii_alphabetic()) && p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
                if name_like && !out.contains(&p) {
                    out.push(p);
                }
            }
        }
    }
    out
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
            } else if generated_name(t) {
                "_name"
            } else {
                t
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A name with a generated suffix: a pod or container (`api-7f9c4`, `web-6d4cf56db6-x2k1m`).
fn generated_name(t: &str) -> bool {
    let Some((head, tail)) = t.rsplit_once('-') else { return false };
    !head.is_empty()
        && head.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && head.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && (4..=10).contains(&tail.len())
        && tail.chars().all(|c| c.is_ascii_alphanumeric())
        && tail.chars().any(|c| c.is_ascii_digit())
        && tail.chars().any(|c| c.is_ascii_alphabetic())
}

/// A command with one blank: variants of one command that differ in exactly one place, after
/// the program and subcommand, never in a flag (`git commit -m "‹message›"`, `kubectl logs ‹name›`).
#[derive(Debug, PartialEq)]
pub struct Template {
    /// the blank shown as ‹kind›
    pub display: String,
    /// for the prompt: \u{1} where the cursor goes, inside the quotes when the blank was quoted
    pub fill: String,
    pub kind: &'static str,
    /// what went in the blank, in the order given (most recent first), at most 5
    pub values: Vec<String>,
}

/// The template of `variants` (most recent first), or None when they don't differ in exactly one
/// such place.
pub fn template(variants: &[&str]) -> Option<Template> {
    let toks: Vec<Vec<String>> = variants.iter().map(|v| words_quoted(v)).collect();
    let n = toks.first()?.len();
    if toks.len() < 2 || toks.iter().any(|t| t.len() != n) {
        return None;
    }
    let diff: Vec<usize> = (0..n).filter(|&i| toks.iter().any(|t| t[i] != toks[0][i])).collect();
    let [p] = diff[..] else { return None };
    // never the program or its subcommand: `docker compose up` / `down` are two commands
    if p < group_key(variants[0]).split_whitespace().count().max(1) {
        return None;
    }
    let vals: Vec<&str> = toks.iter().map(|t| t[p].as_str()).collect();
    // the blank holds data, as folding sees it (quoted text, a path, a number or hash, a
    // generated name), never a flag or a word like `up` / `down`
    if vals.iter().any(|v| v.starts_with('-') || !matches!(shape_key(v).as_str(), "\"_\"" | "_path" | "_n" | "_name")) {
        return None;
    }
    // quoted, each its own way (`"wip"`, `'fix login'`): the blank takes the most recent's quotes
    let in_quotes = |v: &str| v.len() >= 2 && ['"', '\''].iter().any(|q| v.starts_with(*q) && v.ends_with(*q));
    let quoted = vals[0].chars().next().filter(|_| vals.iter().all(|v| in_quotes(v)));
    let prev = toks[0][p - 1].to_lowercase();
    let kube = matches!(toks[0][0].to_lowercase().trim_end_matches(".exe"), "kubectl" | "helm" | "oc" | "k");
    let kind = match prev.as_str() {
        "-m" | "--message" => "message",
        "-n" | "--namespace" if kube => "namespace",
        "checkout" | "switch" | "merge" | "rebase" => "branch",
        "cd" | "set-location" | "sl" | "pushd" | "chdir" => "folder",
        _ if vals.iter().all(|v| v.contains('/') || v.contains('\\')) => "path",
        _ if vals.iter().all(|v| v.chars().all(|c| c.is_ascii_digit())) => "number",
        _ if vals.iter().all(|v| generated_name(&v.to_lowercase())) => "name",
        _ if quoted.is_some() => "text",
        _ => "value",
    };
    let put = |blank: &str| {
        let mut t = toks[0].clone();
        t[p] = match quoted {
            Some(q) => format!("{q}{blank}{q}"),
            None => blank.to_string(),
        };
        t.join(" ")
    };
    let mut values: Vec<String> = Vec::new();
    for v in &vals {
        let v = match quoted {
            Some(_) => v[1..v.len() - 1].to_string(),
            None => v.to_string(),
        };
        if !values.contains(&v) && values.len() < 5 {
            values.push(v);
        }
    }
    Some(Template { display: put(&format!("‹{kind}›")), fill: put("\u{1}"), kind, values })
}

/// Words of a command line, a quoted string being one word (quotes kept).
fn words_quoted(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in cmd.trim().chars() {
        match quote {
            Some(q) => {
                cur.push(c);
                if c == q {
                    quote = None;
                }
            }
            None if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            None => {
                if c == '"' || c == '\'' {
                    quote = Some(c);
                }
                cur.push(c);
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
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

/// command_desc_vec is keyed (command, kind): one kind per description sentence, max-blended
const SPEC: [&str; 4] = ["spec", "spec2", "spec3", "spec4"];
const WSPEC: [&str; 4] = ["wspec", "wspec2", "wspec3", "wspec4"];

pub fn describe(cmd: &str) -> Description {
    let mut d = describe_one(cmd);
    // the wrapped command says what it's FOR (`alembic upgrade head` = migrations), so it leads
    if let Some(inner) = wrapped(cmd) {
        let i = describe_one(&inner);
        if !i.parts.is_empty() {
            d.display = i.display.clone().or(d.display);
            // own kinds: command_desc_vec is keyed (command, kind)
            let own = |k: &str| if k == "gen" { "wgen" } else { SPEC.iter().position(|s| *s == k).map_or("wspec", |i| WSPEC[i]) };
            let mut parts: Vec<(&'static str, String)> = i.parts.into_iter().map(|(k, t)| (own(k), t)).collect();
            parts.extend(d.parts);
            d.parts = parts;
        }
    }
    d
}

/// The subcommand of a subcommand: the first bare word after `sub`, past any flags
/// (`docker compose -f x.yml down` -> `down`). None when something else comes first.
fn word_after(cmd: &str, sub: &str) -> Option<String> {
    let mut toks = cmd.split_whitespace().map(str::to_lowercase).skip_while(|t| t != sub).skip(1);
    toks.find(|t| !t.starts_with('-') && !t.contains('.')).filter(|t| is_bare_word(t))
}

fn add<'a>(spec: &mut Vec<&'a String>, g: Option<&'a String>) {
    if let Some(g) = g {
        if !spec.iter().any(|s| s.eq_ignore_ascii_case(g)) {
            spec.push(g);
        }
    }
}

/// The argument after `word` (the program, or its subcommand): `git reset --soft HEAD~1` after
/// `reset` is `--soft`.
fn arg_after(cmd: &str, word: &str) -> Option<String> {
    let norm = |t: &str| {
        let t = t.trim_matches(|c| c == '"' || c == '\'').replace('\\', "/").to_lowercase();
        let t = t.rsplit('/').next().unwrap_or("").to_string();
        alias(t.strip_suffix(".exe").unwrap_or(&t)).to_string()
    };
    let mut toks = cmd.split_whitespace();
    toks.find(|t| norm(t) == word)?;
    toks.next().map(str::to_lowercase)
}

fn describe_one(cmd: &str) -> Description {
    let (prog, sub) = parse_cmd(cmd);
    let Some(prog) = prog.filter(|p| !NON_TOOLS.contains(&p.as_str())) else {
        return Description { display: None, parts: vec![] };
    };
    let m = tldr();
    // what the first argument says: `git checkout main` "Switch to an existing local branch",
    // `git reset --soft` "Undo the last commit...", `ipconfig /flushdns` "Remove all data from
    // the local DNS cache" (tldr examples keyed by flag, switch, or `*` for a plain value)
    let by_arg = |page: &str, after: &str| -> Option<&String> {
        let arg = arg_after(cmd, after)?;
        let key = if arg.starts_with('-') || arg.starts_with('/') { arg } else { "*".to_string() };
        m.ex.get(&format!("{page}\0{key}"))
    };
    let mut parts = Vec::new();
    if let Some(g) = m.cmd.get(&prog) {
        parts.push(("gen", format!("{prog}: {g}")));
    }
    let mut spec: Vec<&String> = Vec::new();
    // the words of the command the sentences describe: `docker compose down`
    let mut label = prog.clone();
    if let Some(sub) = sub {
        // two-level tools: `docker compose down` is about `down`, not "run and manage multi
        // container applications" - use the deeper page when tldr has one
        let page = format!("{prog}-{sub}");
        if let Some(sub2) = word_after(cmd, &sub) {
            add(&mut spec, m.cmd.get(&format!("{page}-{sub2}")));
            add(&mut spec, m.ex.get(&format!("{page}\0{sub2}")));
            if !spec.is_empty() {
                label = format!("{prog} {sub} {sub2}");
            }
        }
        if spec.is_empty() {
            add(&mut spec, m.cmd.get(&page));
            add(&mut spec, m.ex.get(&format!("{prog}\0{sub}")));
            add(&mut spec, by_arg(&page, &sub));
            label = format!("{prog} {sub}");
        }
    } else {
        add(&mut spec, by_arg(&prog, &prog));
    }
    let display = if spec.is_empty() {
        parts.first().map(|p| p.1.clone())
    } else {
        Some(format!("{prog}: {}", spec.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("; ")))
    };
    // each sentence embedded on its own, named by the command it describes: joined, or behind
    // a bare "docker-compose:", it matches less ("tear down containers": 0.666 -> 0.709)
    for (kind, s) in SPEC.iter().zip(&spec) {
        parts.push((*kind, format!("{label}: {s}")));
    }
    Description { display, parts }
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
    fn templates_with_one_blank() {
        let t = template(&[r#"git commit -m "fix login""#, r#"git commit -m "wip""#, r#"git commit -m "wip""#]).unwrap();
        assert_eq!((t.display.as_str(), t.fill.as_str(), t.kind), (r#"git commit -m "‹message›""#, "git commit -m \"\u{1}\"", "message"));
        assert_eq!(t.values, vec!["fix login", "wip"]);
        let t = template(&["kubectl logs api-7f9c4 -n prod", "kubectl logs web-x2k1m -n prod"]).unwrap();
        assert_eq!((t.display.as_str(), t.kind), ("kubectl logs ‹name› -n prod", "name"));
        assert_eq!(template(&[r"cd D:\work\api", r"cd D:\work\web"]).map(|t| t.fill), Some("cd \u{1}".into()));
        assert_eq!(template(&["docker compose up", "docker compose down"]), None, "the subcommand differs");
        assert_eq!(template(&["git commit -m a -q", "git commit -m b -v"]), None, "two places differ");
        assert_eq!(template(&["ls -la", "ls -l"]), None, "a flag isn't a blank");
        assert_eq!(template(&["git status"]), None, "one variant is just a command");
        assert_eq!(template(&[r#"test -n "a1""#, r#"test -n "b2""#]).map(|t| t.kind), Some("text"), "-n is a namespace only for kubectl");
        let t = template(&[r#"git commit -m "Move""#, "git commit -m 'First commit'"]).unwrap();
        assert_eq!((t.fill.as_str(), t.values.clone()), ("git commit -m \"\u{1}\"", vec!["Move".to_string(), "First commit".to_string()]), "quotes of either kind");
        assert_eq!(template(&["kubectl get pods -n api-7f9c4", "kubectl get pods -n web-x2k1m"]).map(|t| t.kind), Some("namespace"));
        assert_eq!(shape_key("kubectl logs api-7f9c4 -n prod"), shape_key("kubectl logs web-6d4cf56db6-x2k1m -n prod"));
        assert_ne!(shape_key("node-18 x"), shape_key("node-20 x"), "a short suffix is a version, not a generated name");
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
        assert_eq!(wrapped("docker exec api_web alembic upgrade head").as_deref(), Some("alembic upgrade head"));
        assert_eq!(wrapped("docker exec -it -u postgres db psql -U x").as_deref(), Some("psql -U x"));
        assert_eq!(wrapped("docker compose exec web alembic upgrade head").as_deref(), Some("alembic upgrade head"));
        assert_eq!(wrapped("docker-compose run --rm web pytest -q").as_deref(), Some("pytest -q"));
        assert_eq!(wrapped("docker compose -f prod.yml exec web alembic current").as_deref(), Some("alembic current"));
        assert_eq!(wrapped("kubectl exec -it api-7f -- python manage.py migrate").as_deref(), Some("python manage.py migrate"));
        assert_eq!(wrapped("python -m pytest tests").as_deref(), Some("pytest tests"));
        assert_eq!(wrapped("docker compose up -d"), None);
        assert_eq!(wrapped("git status"), None);
        let d = describe("docker exec api_web alembic upgrade head");
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

#[cfg(test)]
mod program_tests {
    use super::programs;

    #[test]
    fn every_step_and_what_wrappers_run() {
        assert_eq!(programs("npx astro dev --port 4321"), ["npx", "astro"]);
        assert_eq!(programs("cd D:/portfolio && npx vercel --prod 2>&1 | tail -3"), ["npx", "vercel", "tail"]);
        assert_eq!(programs("git add -A; git commit -m wip"), ["git"]);
        assert_eq!(programs("sudo docker ps"), ["sudo", "docker"]);
        // a cd step runs nothing; variables and numbers are not programs
        assert!(programs("cd portfolio").is_empty());
        assert!(programs("$x = 1").is_empty());
    }
}
