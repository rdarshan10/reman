//! Offline, no-execution command understanding: program/subcommand parsing, variant grouping,
//! tldr-pages descriptions. Port of reman_enrich.py (parse_cmd, describe, group_key) - nothing
//! here ever runs a binary. (Repo identity is in git.rs.)
use std::collections::HashMap;
use std::sync::OnceLock;

static TLDR_JSON: &[u8] = include_bytes!("../../tldr_map.json");

struct TldrMap {
    cmd: HashMap<String, String>,
    /// page -> its examples, in order (tools/build_tldr_map.py)
    ex: HashMap<String, Vec<Example>>,
}

/// One tldr example: what it does, the words a command needs to be it (space-separated,
/// alternatives joined by `|`: `-itemtype directory`, `-d|--delete`), and whether it takes a value.
struct Example {
    desc: Box<str>,
    words: Box<str>,
    value: bool,
}

impl Example {
    fn words(&self) -> impl Iterator<Item = &str> {
        self.words.split(' ').filter(|w| !w.is_empty())
    }
}

fn tldr() -> &'static TldrMap {
    static M: OnceLock<TldrMap> = OnceLock::new();
    M.get_or_init(|| {
        #[derive(serde::Deserialize)]
        struct Raw {
            cmd: HashMap<String, String>,
            ex: HashMap<String, Vec<(String, Vec<String>, u8)>>,
        }
        let raw: Raw = serde_json::from_slice(TLDR_JSON).unwrap_or(Raw { cmd: HashMap::new(), ex: HashMap::new() });
        let ex = raw
            .ex
            .into_iter()
            .map(|(page, exs)| (page, exs.into_iter().map(|(d, w, v)| Example { desc: d.into(), words: w.join(" ").into(), value: v != 0 }).collect()))
            .collect();
        TldrMap { cmd: raw.cmd, ex }
    })
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

/// The command without a prompt pasted in front of it: `C:\Windows\System32>netstat -ano`,
/// `PS C:\code> npm test`.
fn without_prompt(cmd: &str) -> &str {
    let t = cmd.trim_start();
    let rest = t.strip_prefix("PS ").unwrap_or(t);
    let b = rest.as_bytes();
    if b.len() > 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/') {
        if let Some(i) = rest.find('>').filter(|&i| !rest[..i].contains(char::is_whitespace) || t.starts_with("PS ")) {
            return rest[i + 1..].trim_start();
        }
    }
    cmd
}

/// (program, subcommand) with leading `cd x;`, `&`, drive hops etc. skipped.
pub fn parse_cmd(cmd: &str) -> (Option<String>, Option<String>) {
    let toks: Vec<&str> = without_prompt(cmd).split_whitespace().collect();
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
    let cmd = without_heredoc_bodies(cmd);
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

/// The command without what a heredoc (`<<'EOF'` ... `EOF`) or a PowerShell here-string (`@'` ...
/// `'@`) feeds it: that is data (a script, a file's text), not commands. A pasted Python script's
/// `("tear down containers", ...)` would otherwise make `tear` a program, and a search naming it a
/// search for that tool.
fn without_heredoc_bodies(cmd: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut until: Option<String> = None;
    for line in cmd.lines() {
        if let Some(end) = &until {
            if line.trim() == end {
                until = None;
            }
            continue;
        }
        out.push(line);
        let t = line.trim_end();
        if let Some(i) = t.find("<<").filter(|&i| !t[i + 2..].starts_with('<')) {
            let tag: String = t[i + 2..].trim_start_matches('-').trim_start().trim_start_matches(['\'', '"']).chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
            if !tag.is_empty() {
                until = Some(tag);
            }
        } else if t.ends_with("@'") || t.ends_with("@\"") {
            until = Some(if t.ends_with("@'") { "'@" } else { "\"@" }.to_string());
        }
    }
    out.join("\n")
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
    // `npx install x` runs the npm package called install, not the Unix tool of that name: an
    // npm word after npx is a slip, not a program to describe
    let npm_word = matches!(p0, "npx" | "pnpx" | "bunx")
        && matches!(inner.split_whitespace().next().map(str::to_lowercase).as_deref(), Some("install" | "i" | "add" | "run" | "exec" | "init" | "update" | "link" | "ci" | "test" | "start"));
    (!inner.trim().is_empty() && !npm_word).then_some(inner)
}

/// What a command, or a request, does to its object. tldr's sentences start with it ("Remove
/// files", "Install Node packages", "List all branches"), and so do most requests ("install node
/// modules", "make a branch"): a request to install is not answered by `rm node_modules`, however
/// alike the words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Create,
    Delete,
    Install,
    Start,
    Stop,
    Show,
    Change,
    Download,
    Upload,
    Pack,
    Unpack,
}

/// The action a verb names, if any.
pub fn action_of(word: &str) -> Option<Action> {
    use Action::*;
    Some(match word {
        "create" | "creates" | "make" | "new" | "generate" | "init" | "initialize" | "scaffold" | "mkdir" => Create,
        "delete" | "deletes" | "remove" | "removes" | "rm" | "erase" | "clean" | "clear" | "purge" | "discard" | "destroy" | "uninstall" | "prune" | "wipe" | "drop" => Delete,
        "install" | "installs" | "reinstall" => Install,
        "start" | "starts" | "run" | "runs" | "launch" | "serve" | "execute" | "boot" | "open" => Start,
        "stop" | "stops" | "kill" | "terminate" | "halt" | "shutdown" | "quit" | "close" | "end" => Stop,
        "list" | "lists" | "show" | "shows" | "see" | "display" | "view" | "print" | "check" | "inspect" | "get" => Show,
        "set" | "change" | "configure" | "edit" | "modify" | "rename" | "update" | "upgrade" => Change,
        "download" | "fetch" | "pull" | "clone" => Download,
        "upload" | "push" | "publish" | "deploy" => Upload,
        "compress" | "zip" | "archive" | "pack" => Pack,
        "extract" | "unzip" | "unpack" | "decompress" | "expand" => Unpack,
        _ => return None,
    })
}

/// The action a request names: its first verb, phrasal ones included (`spin up`, `tear down`).
pub fn request_action(text: &str) -> Option<Action> {
    let words: Vec<String> = text.to_lowercase().split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).map(String::from).collect();
    // a question asks to be shown: "what files did i modify" changes nothing
    if words.first().is_some_and(|w| matches!(w.as_str(), "what" | "whats" | "which" | "where" | "who" | "when")) {
        return Some(Action::Show);
    }
    // "how do i install x", "how to install x": the verb after
    let skip = words.iter().take_while(|w| matches!(w.as_str(), "how" | "do" | "does" | "can" | "could" | "should" | "would" | "i" | "we" | "to" | "please")).count();
    for (i, w) in words.iter().enumerate().skip(skip) {
        let next = words.get(i + 1).map(String::as_str);
        let phrasal = match (w.as_str(), next) {
            ("spin" | "fire" | "bring", Some("up")) => Some(Action::Start),
            ("tear" | "shut" | "bring", Some("down")) => Some(Action::Stop),
            ("set", Some("up")) => Some(Action::Create),
            ("back", Some("up")) => return None, // a backup copies; it clashes with nothing
            _ => None,
        };
        if let Some(a) = phrasal.or_else(|| action_of(w)) {
            return Some(a);
        }
    }
    None
}

/// The action a command's description names: the first verb its specific sentences start with
/// (the wrapped command's first: `docker exec web alembic upgrade head` upgrades). Never its
/// program's in general: `npx kill-port 3000` is no "Execute binaries from npm packages".
pub fn command_action(cmd: &str, d: &Description) -> Option<Action> {
    // a pipeline or a chain does what its last step does, which its description doesn't say
    // (`git branch --merged main | xargs git branch -d` lists, then deletes)
    if cmd.trim().contains(['|', ';', '\n']) || cmd.contains("&&") {
        return None;
    }
    let verb = |text: &str| {
        let sentence = text.split_once(": ").map_or(text, |(_, s)| s);
        sentence.split_whitespace().next().and_then(|w| action_of(&w.to_lowercase()))
    };
    d.parts.iter().filter(|p| p.0 != "gen" && p.0 != "wgen").find_map(|p| verb(&p.1))
}

/// Requests and commands that undo each other, or read where the other writes: "install node
/// modules" is not answered by `rm node_modules`, "list containers" not by `docker run`.
pub fn clash(a: Action, b: Action) -> bool {
    use Action::*;
    let pair = |x: Action, y: Action| (a == x && b == y) || (a == y && b == x);
    pair(Create, Delete)
        || pair(Install, Delete)
        || pair(Start, Stop)
        || pair(Download, Upload)
        || pair(Pack, Unpack)
        || pair(Create, Unpack)
        || (a == Show || b == Show) && matches!(if a == Show { b } else { a }, Delete | Stop | Create | Install | Change | Start)
}

/// command_desc_vec is keyed (command, kind): one kind per description sentence, max-blended
const SPEC: [&str; 4] = ["spec", "spec2", "spec3", "spec4"];
const WSPEC: [&str; 4] = ["wspec", "wspec2", "wspec3", "wspec4"];

/// What a pipeline ending in `xargs <command>` runs on what came before it:
/// `git branch --merged main | grep -v main | xargs git branch -d` deletes branches.
fn xargs_step(cmd: &str) -> Option<String> {
    if cmd.contains('\n') {
        return None;
    }
    let last = cmd.rsplit_once('|')?.1.trim();
    let mut toks = last.split_whitespace();
    if toks.next()? != "xargs" {
        return None;
    }
    let mut rest: Vec<&str> = Vec::new();
    let mut skip_value = false;
    for t in toks {
        if rest.is_empty() && (skip_value || t.starts_with('-')) {
            // xargs's own options; these take a value
            skip_value = !skip_value && matches!(t, "-n" | "-I" | "-L" | "-P" | "-d" | "-E" | "-s");
            continue;
        }
        rest.push(t);
    }
    (!rest.is_empty()).then(|| rest.join(" "))
}

pub fn describe(cmd: &str) -> Description {
    let mut d = describe_one(cmd);
    // the wrapped command says what it's FOR (`alembic upgrade head` = migrations), so it leads;
    // so does what a pipeline's `xargs` step does
    if let Some(inner) = wrapped(cmd).or_else(|| xargs_step(cmd)) {
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

fn add(spec: &mut Vec<String>, g: Option<&str>) {
    if let Some(g) = g {
        if !spec.iter().any(|s| s.eq_ignore_ascii_case(g)) {
            spec.push(g.to_string());
        }
    }
}

/// The words after `word` (the program, or its subcommand), as tldr examples spell them, up to
/// the end of that command (`|`, `&&`, `;`): `git branch -D old` after `branch` is `-d`, `old`.
/// `--flag=value` is `--flag`; a cluster of short flags is each of them too (`-rf`: -r, -f).
fn args_after(cmd: &str, word: &str) -> Vec<String> {
    let norm = |t: &str| {
        let t = t.trim_matches(|c| c == '"' || c == '\'').replace('\\', "/").to_lowercase();
        let t = t.rsplit('/').next().unwrap_or("").to_string();
        alias(t.strip_suffix(".exe").unwrap_or(&t)).to_string()
    };
    let mut toks = cmd.split_whitespace();
    if toks.find(|t| norm(t) == word).is_none() {
        return vec![];
    }
    let mut out = Vec::new();
    for t in toks {
        if matches!(t, "|" | "||" | "&&" | "&" | ";" | ">" | ">>" | "2>&1") {
            break;
        }
        let a = t.trim_end_matches(';').trim_matches(|c| c == '"' || c == '\'').to_lowercase();
        let a = a.split_once('=').map_or(a.as_str(), |(k, _)| if k.starts_with('-') { k } else { a.as_str() }).to_string();
        let cluster = a.len() > 2 && a.len() <= 4 && a.starts_with('-') && a[1..].bytes().all(|c| c.is_ascii_lowercase());
        if cluster {
            out.extend(a[1..].chars().map(|c| format!("-{c}")));
        }
        out.push(a);
        if t.ends_with(';') {
            break;
        }
    }
    out
}

/// The example of `page` that describes a command whose words after the page's own are `args`:
/// the one with the most words, all of them in the command (`New-Item -ItemType Directory x`:
/// "Create a new directory", not the `-ItemType SymbolicLink` one). Failing that, a command of
/// values only takes the page's first example of values only (`git checkout main`: "Switch to an
/// existing local branch"), and a bare command the first with no arguments - but never on a page
/// of subcommands: `docker POSTGRES_USER=x psql` is no `docker ps`.
fn example_for<'a>(m: &'a TldrMap, page: &str, args: &[String]) -> Option<&'a str> {
    let exs = m.ex.get(page)?;
    // where in the command a word is: of two examples as long, the one for what comes first
    // (`rm -rf`: recursive, not force)
    let at = |w: &str| w.split('|').filter_map(|alt| args.iter().position(|a| a == alt)).min();
    let best = exs
        .iter()
        .enumerate()
        .filter(|(_, e)| e.words().next().is_some() && e.words().all(|w| at(w).is_some()))
        .max_by_key(|(i, e)| (e.words().count(), std::cmp::Reverse(e.words().filter_map(at).min()), std::cmp::Reverse(*i)));
    if let Some((_, e)) = best {
        return Some(&e.desc);
    }
    match args.first() {
        None => exs.iter().find(|e| e.words().next().is_none() && !e.value),
        Some(a) if !a.starts_with(['-', '/']) && !exs.iter().any(|e| e.words().next().is_some_and(|w| !w.starts_with(['-', '/']))) => {
            exs.iter().find(|e| e.words().next().is_none() && e.value)
        }
        Some(_) => None,
    }
    .map(|e| &*e.desc)
}

/// The first example of `page` led by `word` whose other words the command has too (`args`, its
/// words after `word`): `docker compose` -> its `compose` example; `docker ps` is no `docker ps
/// -a`, "List all Docker containers (running and stopped)".
fn led_by<'a>(m: &'a TldrMap, page: &str, word: &str, args: &[String]) -> Option<&'a str> {
    let has = |w: &str| w.split('|').any(|alt| args.iter().any(|a| a == alt));
    m.ex.get(page)?
        .iter()
        .find(|e| {
            let mut ws = e.words();
            ws.next().is_some_and(|w| w.split('|').any(|a| a == word)) && ws.all(has)
        })
        .map(|e| &*e.desc)
}

/// Setting an environment variable, which no tldr page describes: PowerShell's `$env:NAME = ...`,
/// cmd's `set NAME=...`, a POSIX shell's `export NAME=...`. (label, sentence)
fn env_assignment(cmd: &str) -> Option<(String, String)> {
    let t = cmd.trim();
    let name_ok = |n: &str| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if let Some(rest) = t.strip_prefix("$env:").or_else(|| t.strip_prefix("$Env:")).or_else(|| t.strip_prefix("$ENV:")) {
        let (name, after) = rest.split_once('=')?;
        let name = name.trim();
        return (name_ok(name) && !after.starts_with('=')).then(|| ("$env".to_string(), format!("Set the environment variable {name} for this PowerShell session")));
    }
    for (kw, label, sentence) in [("set ", "set", "for this Command Prompt session"), ("export ", "export", "for this shell and the programs it starts")] {
        if let Some(rest) = t.get(..kw.len()).filter(|p| p.eq_ignore_ascii_case(kw)).map(|_| &t[kw.len()..]) {
            let name = rest.split_once('=')?.0.trim();
            return name_ok(name).then(|| (label.to_string(), format!("Set the environment variable {name} {sentence}")));
        }
    }
    None
}

/// Changing directory, the commonest line of all, which no tldr page describes the way people ask
/// for it ("go up one directory"). Only a line that does nothing else: `cd api && npm test` is
/// about the test.
fn directory_change(cmd: &str) -> Option<(String, String)> {
    let t = cmd.trim();
    if t.contains(['\n', ';', '|', '&']) {
        return None;
    }
    let mut toks = t.split_whitespace();
    let verb = toks.next()?.to_lowercase();
    if !matches!(verb.as_str(), "cd" | "chdir" | "set-location" | "sl" | "pushd" | "popd") {
        return None;
    }
    // flags out (`cd /d`, `Set-Location -Path`), but `-` alone is "where I was"
    let args: Vec<&str> = toks.filter(|a| *a == "-" || !(a.starts_with('-') || a.eq_ignore_ascii_case("/d"))).collect();
    let sentence = match (verb.as_str(), args.as_slice()) {
        ("popd", _) => "Go back to the directory saved by pushd",
        (_, [".."]) | (_, ["../"]) | (_, ["..\\"]) => "Go up one directory, to the parent folder",
        (_, ["-"]) => "Go back to the previous directory",
        (_, []) | (_, ["~"]) => "Go to the home directory",
        (_, [a]) if a.starts_with("..") => "Go up to a parent directory",
        _ => "Change the current directory: go into a folder",
    };
    Some((verb, sentence.to_string()))
}

fn describe_one(cmd: &str) -> Description {
    let cmd = without_prompt(cmd);
    if let Some((label, sentence)) = env_assignment(cmd).or_else(|| directory_change(cmd)) {
        return Description { display: Some(format!("{label}: {sentence}")), parts: vec![("spec", format!("{label}: {sentence}"))] };
    }
    let (prog, sub) = parse_cmd(cmd);
    let Some(prog) = prog.filter(|p| !NON_TOOLS.contains(&p.as_str())) else {
        return Description { display: None, parts: vec![] };
    };
    let m = tldr();
    let mut parts = Vec::new();
    if let Some(g) = m.cmd.get(&prog) {
        parts.push(("gen", format!("{prog}: {g}")));
    }
    // what the arguments say: `git checkout main` "Switch to an existing local branch",
    // `git reset --soft` "Undo the last commit...", `ipconfig /flushdns` "Remove all data from
    // the local DNS cache" (example_for)
    let mut spec: Vec<String> = Vec::new();
    // the words of the command the sentences describe: `docker compose down`
    let mut label = prog.clone();
    if let Some(sub) = sub {
        // two-level tools: `docker compose down` is about `down`, not "run and manage multi
        // container applications" - use the deeper page when tldr has one
        let page = format!("{prog}-{sub}");
        // the example for this very command when there is one; else the parent page's example of
        // the subcommand (a usage of it, perhaps another: `git branch -D x` is no "List all
        // branches")
        if let Some(sub2) = word_after(cmd, &sub) {
            let page2 = format!("{page}-{sub2}");
            let args2 = args_after(cmd, &sub2);
            match m.cmd.get(&page2) {
                Some(g) => {
                    add(&mut spec, Some(g));
                    add(&mut spec, example_for(m, &page2, &args2));
                }
                // no page of its own (`kubectl get pods`): its example on the page above, after
                // that page's own sentence
                None => {
                    if let Some(e) = led_by(m, &page, &sub2, &args2) {
                        add(&mut spec, m.cmd.get(&page).map(String::as_str));
                        add(&mut spec, Some(e));
                    }
                }
            }
            if !spec.is_empty() {
                label = format!("{prog} {sub} {sub2}");
            }
        }
        if spec.is_empty() {
            let args = args_after(cmd, &sub);
            add(&mut spec, m.cmd.get(&page).map(String::as_str));
            match example_for(m, &page, &args) {
                Some(e) => add(&mut spec, Some(e)),
                None => add(&mut spec, led_by(m, &prog, &sub, &args)),
            }
            label = format!("{prog} {sub}");
        }
        // no subcommand after all (`python run.py`): the program's own examples
        if spec.is_empty() {
            add(&mut spec, example_for(m, &prog, &args_after(cmd, &prog)));
            label = prog.clone();
        }
    } else {
        let args = args_after(cmd, &prog);
        add(&mut spec, example_for(m, &prog, &args));
        // `node -v`, `python --version`: tldr seldom has an example for it
        if spec.is_empty() && args.len() == 1 && matches!(args[0].as_str(), "-v" | "--version" | "-version" | "/version") {
            spec.push(format!("Show the installed version of {prog}"));
        }
    }
    let display = if spec.is_empty() {
        parts.first().map(|p| p.1.clone())
    } else {
        Some(format!("{prog}: {}", spec.join("; ")))
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
    fn heredoc_bodies_are_not_programs() {
        let py = "python - <<'EOF'\nrows = [\n    (\"tear down containers\", \"docker compose down\"),\n]\nEOF\nnpm test";
        assert_eq!(programs(py), vec!["python", "npm"]);
        let ps = "$s = @'\nshred everything\n'@\ngit status";
        assert_eq!(programs(ps), vec!["git"]);
        // a here-string (<<<) has no body
        assert_eq!(programs("grep x <<< \"$v\"\nls"), vec!["grep", "ls"]);
    }

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
        assert_eq!(template(&[r"cd C:\code\api", r"cd C:\code\web"]).map(|t| t.fill), Some("cd \u{1}".into()));
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

    #[test]
    fn what_a_request_and_a_command_do() {
        use Action::*;
        assert_eq!(request_action("install node modules"), Some(Install));
        assert_eq!(request_action("tear down the containers"), Some(Stop));
        assert_eq!(request_action("spin up docker"), Some(Start));
        assert_eq!(request_action("how do i delete a branch"), Some(Delete));
        // a question asks to be shown, whatever its verb
        assert_eq!(request_action("what files did i modify"), Some(Show));
        assert_eq!(request_action("back up the database"), None);
        assert_eq!(request_action("git status"), None);
        assert_eq!(command_action("rm -rf node_modules", &describe("rm -rf node_modules")), Some(Delete));
        assert_eq!(command_action("npm install", &describe("npm install")), Some(Install));
        assert_eq!(command_action("git branch -D old", &describe("git branch -D old")), Some(Delete));
        // a pipeline does what its last step does: not known
        let p = "git branch --merged main | grep -v main | xargs git branch -d";
        assert_eq!(command_action(p, &describe(p)), None);
        assert!(clash(Install, Delete) && clash(Show, Start) && clash(Start, Stop));
        assert!(!clash(Install, Install) && !clash(Show, Download));
    }

    fn says(cmd: &str) -> String {
        describe(cmd).display.unwrap_or_default()
    }

    #[test]
    fn the_example_whose_words_the_command_has() {
        // options written {{[-d|--delete]}} in tldr
        assert!(says("git branch -D old-feature").contains("Delete a local branch"), "{}", says("git branch -D old-feature"));
        // every word of the example, not just the first argument (that one is a symlink)
        assert!(says("New-Item -ItemType Directory -Force -Path backups").contains("Create a new directory"));
        // values only
        assert!(says("git checkout main").contains("Switch to an existing local branch"));
        // a file, not a subcommand
        assert!(says("python run.py").contains("Python file"), "{}", says("python run.py"));
        // another command's example on the page (venv's `deactivate`) is not this one's
        assert!(!says("venv").contains("Deactivate"), "{}", says("venv"));
        assert!(says("git reset --soft HEAD~1").contains("Undo"), "{}", says("git reset --soft HEAD~1"));
        // a cluster of short flags is each of them
        assert!(says("rm -rf node_modules").contains("recursively"), "{}", says("rm -rf node_modules"));
        assert!(says("ipconfig /flushdns").to_lowercase().contains("dns"));
    }

    #[test]
    fn nothing_specific_is_made_up() {
        // a garbled paste after a program with subcommands is no `docker ps`
        assert!(!says(r#"docker POSTGRES_USER=x psql -U y -c "CREATE DATABASE t;""#).contains("List all Docker containers"));
        // npx runs the npm package "install", not the Unix tool
        assert!(!says("npx install @scope/some-package").contains("Copy files"));
        // a pipeline does what its xargs step does
        assert!(says("git branch --merged main | grep -v main | xargs git branch -d").contains("Delete a local branch"));
        assert!(says("find . -name '*.tmp' | xargs -n 1 rm").contains("Remove"), "{}", says("find . -name '*.tmp' | xargs -n 1 rm"));
        // `docker ps` is not `docker ps -a`
        assert!(!says("docker ps").contains("running and stopped"), "{}", says("docker ps"));
        assert!(says("docker ps -a").contains("running and stopped"), "{}", says("docker ps -a"));
    }

    #[test]
    fn what_tldr_does_not_say() {
        assert!(says("node -v").contains("version of node"));
        assert!(says("python --version").contains("version of python"));
        assert!(says(r#"$env:API_URL = "http://localhost:8000""#).contains("environment variable API_URL"));
        assert!(says("set NODE_ENV=production").contains("environment variable NODE_ENV"));
        assert!(says("export FOO=1").contains("environment variable FOO"));
        assert!(says("Copy-Item a.txt b.txt").to_lowercase().contains("copy files"));
        assert!(says("cd ..").contains("Go up one directory"));
        assert!(says(r"cd D:\code\billing-api").contains("Change the current directory"));
        assert!(says("Set-Location -").contains("previous directory"));
        // a cd that leads into something else is about that something
        assert!(!says("cd api && npm test").contains("Change the current directory"));
        // a pasted prompt is not part of the command
        assert!(says(r#"C:\Windows\System32>netstat -ano | findstr ":8000""#).starts_with("netstat:"));
        assert_eq!(parse_cmd(r"PS C:\code\api> npm test").0.as_deref(), Some("npm"));
        assert_eq!(parse_cmd(r"C:\Python\python.exe app.py > out.txt").0.as_deref(), Some("python"));
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
        assert_eq!(programs("cd C:/code/site && npx vercel --prod 2>&1 | tail -3"), ["npx", "vercel", "tail"]);
        assert_eq!(programs("git add -A; git commit -m wip"), ["git"]);
        assert_eq!(programs("sudo docker ps"), ["sudo", "docker"]);
        // a cd step runs nothing; variables and numbers are not programs
        assert!(programs("cd site").is_empty());
        assert!(programs("$x = 1").is_empty());
    }
}
