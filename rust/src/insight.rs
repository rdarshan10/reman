//! What the history says about a folder, beyond search:
//!  - `last_visit`: what you did here last time, for the "welcome back" line;
//!  - `what_broke`: a command that kept working here has started failing - what ran here since;
//!  - `runbook`: how this project is run, from the commands that worked here.
//! Pure functions over the in-memory store; the daemon, the CLI and MCP format the results.
use crate::describe;
use crate::flows;
use crate::store::{Agg, Entry, Scope, Store};
use crate::unwrap::{self, Outcome};
use std::collections::HashMap;

/// Looking around, not doing: never part of "what you did here". A line is when everything it
/// ran is (`cd api`, `cd api; git status`); `cd api && npm test` is not.
pub fn trivial(e: &Entry) -> bool {
    e.comment || e.noise || e.self_ref || fragment(&e.text) || unwrap::commands(&e.text).is_none_or(|cs| cs.iter().all(|c| trivial_cmd(&c.main)))
}

/// A line of a pasted block rather than a command: source code (`from PIL import Image`), or a
/// heredoc's first line recorded without its body.
fn fragment(text: &str) -> bool {
    // (a line starting with `.` is a command here: `. .\env.ps1`, `.venv/bin/python`)
    (!text.starts_with('.') && crate::store::looks_like_code(text)) || (text.contains("<<") && !text.contains('\n'))
}

/// One command that only looks around (`git status`, `ls`), or a terminal's own startup line.
fn trivial_cmd(cmd: &str) -> bool {
    let looks = |p: &str| {
        matches!(
            p,
            "cd" | "ls" | "ll" | "dir" | "cls" | "clear" | "pwd" | "echo" | "cat" | "type" | "exit" | "history" | "code" | "explorer"
                | "start" | "open" | "less" | "more" | "head" | "tail" | "which" | "where" | "whoami" | "set-location" | "get-childitem"
                | "get-content" | "gci" | "sl" | "popd" | "pushd" | "man" | "help" | "tldr" | "reman" | "r" | "rr" | "tree" | "du" | "df"
        )
    };
    let g = describe::group_key(cmd);
    let w: Vec<&str> = cmd.split_whitespace().collect();
    let first = w.first().unwrap_or(&"").to_lowercase();
    // asking a tool about itself: `vercel --version`, `go version`, `npm help`
    let about_itself = w.len() <= 3 && w.last().is_some_and(|l| matches!(l.to_lowercase().as_str(), "--version" | "version" | "--help" | "help"));
    about_itself
        || looks(first.strip_suffix(".exe").unwrap_or(&first))
        || looks(g.split(' ').next().unwrap_or(""))
        || matches!(g.as_str(), "git status" | "git log" | "git diff" | "git branch" | "git show" | "git remote" | "docker ps" | "docker images")
        // VS Code's shell integration, run in every terminal it opens
        || cmd.to_lowercase().contains("shellintegration")
}

/// Real work here, by you or an agent: not an agent's one-off (run once by it and never again,
/// usually exploration: `grep ...`, a heredoc edit).
fn worked_here(e: &Entry, cwd: u32) -> bool {
    e.rows.iter().any(|r| r.cwd == Some(cwd) && (r.human > 0 || r.runs > 1))
}

// ---------------------------------------------------------------------------------------------
// last time here
// ---------------------------------------------------------------------------------------------

pub struct Visit {
    /// when you were last here
    pub last: i64,
    /// what you did, oldest first, each command once
    pub steps: Vec<u32>,
}

/// The last stretch of work in this folder (runs here less than an hour apart, walking back
/// from the latest): the commands that worked and did something, each once, oldest first.
pub fn last_visit(st: &Store, cwd: u32, max: usize) -> Option<Visit> {
    const GAP: i64 = 3600;
    let mut here = st.execs.iter().rev().filter(|x| x.cwd == Some(cwd));
    let first = here.next()?;
    let (last, mut prev) = (first.ts, first.ts);
    let mut steps: Vec<u32> = Vec::new();
    for x in std::iter::once(first).chain(here) {
        if prev - x.ts > GAP {
            break;
        }
        prev = x.ts;
        let e = &st.entries[x.entry as usize];
        if x.exit.is_some_and(|c| c != 0) || !e.recallable(false) || trivial(e) || !worked_here(e, cwd) || steps.contains(&x.entry) {
            continue;
        }
        steps.push(x.entry);
        if steps.len() >= max {
            break;
        }
    }
    steps.reverse();
    (steps.len() >= 2).then_some(Visit { last, steps })
}

// ---------------------------------------------------------------------------------------------
// what broke it
// ---------------------------------------------------------------------------------------------

pub struct Broke {
    /// times it worked here before it started failing
    pub worked: u32,
    /// its last success here
    pub last_ok: i64,
    /// its first failure after that
    pub first_fail: i64,
    /// what ran here in between, oldest first: things that change state (dependencies, branch,
    /// schema, files) before the rest
    pub between: Vec<u32>,
    /// (branch, commit) checked out at its last success, and at its latest failure (Exec::branch)
    pub ok_git: (u32, u32),
    pub fail_git: (u32, u32),
}

/// What git says changed between a command's last success and its latest failure, when it knows
/// both: another branch, other commits, or nothing (then the change isn't committed code).
#[derive(Debug, PartialEq)]
pub enum GitChange {
    Branch { worked: String, fails: String },
    Commits { worked: String, fails: String },
    SameCommit,
}

impl GitChange {
    pub fn of(st: &Store, b: &Broke) -> Option<Self> {
        let name = |i: u32| st.git_name(i).map(str::to_string);
        let ((ob, oh), (fb, fh)) = (b.ok_git, b.fail_git);
        if let (Some(w), Some(f)) = (name(ob), name(fb)) {
            if w != f {
                return Some(GitChange::Branch { worked: w, fails: f });
            }
        }
        let short = |c: String| c.chars().take(7).collect::<String>();
        match (name(oh), name(fh)) {
            (Some(w), Some(f)) if w != f => Some(GitChange::Commits { worked: short(w), fails: short(f) }),
            (Some(_), Some(_)) => Some(GitChange::SameCommit),
            _ => None,
        }
    }

    /// One clause for a one-line note.
    pub fn clause(&self) -> String {
        match self {
            GitChange::Branch { worked, fails } => format!("it worked on {worked}, you're on {fails} now"),
            GitChange::Commits { worked, fails } => format!("the code changed since ({worked} → {fails})"),
            GitChange::SameCommit => "same commit as when it worked".to_string(),
        }
    }

    /// Where to look, in a sentence.
    pub fn advice(&self) -> String {
        match self {
            GitChange::Branch { worked, fails } => format!("It worked on branch {worked} and fails on {fails}: compare the two (git diff {worked}...{fails})."),
            GitChange::Commits { worked, fails } => format!("The code changed since it last worked: look at the commits in between (git log {worked}..{fails})."),
            GitChange::SameCommit => "The same commit was checked out when it worked: the change is not in committed code. Look at uncommitted edits, dependencies and the environment.".to_string(),
        }
    }
}

/// Strong evidence a command is about to fail here, worth holding the Enter key for: it never
/// worked here (2+ failures), or it failed the last 3 times in a row. Weaker evidence says
/// nothing - a warning that shows often gets ignored.
#[derive(Debug, PartialEq)]
pub struct Warning {
    /// failures counted: all of them, or the streak
    pub failed: u32,
    /// successes here before the streak (0: it never worked here)
    pub worked: u32,
}

pub fn warning(st: &Store, entry: u32, cwd: u32) -> Option<Warning> {
    // on every Enter: most commands never failed twice here, which the folder's tally says at once
    if st.entries[entry as usize].rows.iter().filter(|r| r.cwd == Some(cwd)).map(|r| r.fail).sum::<u32>() < 2 {
        return None;
    }
    let mine: Vec<bool> = st.execs.iter().filter(|x| x.entry == entry && x.cwd == Some(cwd)).filter_map(|x| x.exit.filter(|e| *e >= 0).map(|e| e == 0)).collect();
    let worked = mine.iter().filter(|ok| **ok).count() as u32;
    let streak = mine.iter().rev().take_while(|ok| !**ok).count() as u32;
    match (worked, streak) {
        (0, f) if f >= 2 => Some(Warning { failed: f, worked: 0 }),
        (w, f) if f >= 3 => Some(Warning { failed: f, worked: w }),
        _ => None,
    }
}

/// Whether failing costs enough to hold Enter for: its failures here took 10s or more, or it
/// changes things (an install, a migration, `rm`), or it sets up, builds, migrates or deploys.
/// A command that fails in a blink isn't held: the fix line after it is enough.
pub fn costly(st: &Store, entry: u32, cwd: u32) -> bool {
    const SLOW_MS: u32 = 10_000;
    let e = &st.entries[entry as usize];
    st.typical(e, Scope::Folder(cwd), false).is_some_and(|ms| ms >= SLOW_MS)
        || changes_things(&e.gkey)
        || section_of(&e.text).is_some_and(|s| matches!(s, 0 | 4 | 5 | 6))
}

/// A command whose outcome here keeps flipping, both ways, with nothing visible changing in
/// between: no new commit, and no command that changes things (an install, a pull, a migration)
/// ran here. Edits between runs aren't visible, so the bar is high: an ordinary fail, fix, pass
/// loop flips once or twice, a flaky command keeps flipping.
#[derive(Debug, PartialEq)]
pub struct Flaky {
    /// of its last runs here
    pub worked: u32,
    pub runs: u32,
}

pub fn flaky(st: &Store, entry: u32, cwd: u32) -> Option<Flaky> {
    const LAST: usize = 20;
    let all: Vec<usize> = (0..st.execs.len()).filter(|&k| {
        let x = &st.execs[k];
        x.entry == entry && x.cwd == Some(cwd) && x.exit.is_some_and(|e| e >= 0)
    }).collect();
    let idx = &all[all.len().saturating_sub(LAST)..];
    if idx.len() < 6 {
        return None;
    }
    let ok = |k: usize| st.execs[k].exit == Some(0);
    let (mut up, mut down) = (0u32, 0u32);
    for w in idx.windows(2) {
        let (p, q) = (w[0], w[1]);
        if ok(p) == ok(q) {
            continue;
        }
        let (hp, hq) = (st.execs[p].head, st.execs[q].head);
        let committed = hp != 0 && hq != 0 && hp != hq;
        let changed = st.execs[p + 1..q].iter().any(|x| x.cwd == Some(cwd) && changes_things(&st.entries[x.entry as usize].gkey));
        if !committed && !changed {
            if ok(q) { up += 1 } else { down += 1 }
        }
    }
    let runs = idx.len() as u32;
    let worked = idx.iter().filter(|&&k| ok(k)).count() as u32;
    let flips = up + down;
    (up > 0 && down > 0 && flips >= 4 && flips * 10 >= runs * 4).then_some(Flaky { worked, runs })
}

impl Flaky {
    pub fn summary(&self) -> String {
        format!("it worked {} of its last {} runs, passing and failing with nothing changed in between", self.worked, self.runs)
    }
}

/// Commands that change what the next command sees.
pub fn changes_things(gkey: &str) -> bool {
    let mut w = gkey.split(' ');
    let (p, s) = (w.next().unwrap_or(""), w.next().unwrap_or(""));
    match p {
        "git" => matches!(s, "pull" | "merge" | "rebase" | "checkout" | "switch" | "reset" | "stash" | "cherry-pick" | "revert" | "am" | "apply" | "clean"),
        "npm" | "pnpm" | "yarn" | "bun" => matches!(s, "install" | "i" | "ci" | "add" | "update" | "upgrade" | "up" | "remove" | "rm" | "uninstall" | "link") || (p == "yarn" && s.is_empty()),
        "pip" | "uv" | "poetry" | "pipenv" | "conda" | "cargo" | "go" | "bundle" | "composer" | "gem" | "brew" | "apt" | "apt-get" | "choco" | "winget" | "scoop"
        | "dotnet" | "rustup" | "nvm" | "pyenv" => {
            matches!(s, "install" | "add" | "update" | "upgrade" | "remove" | "uninstall" | "sync" | "get" | "lock" | "restore" | "use" | "default")
        }
        "docker" | "docker-compose" => matches!(s, "compose" | "pull" | "build" | "rm" | "volume" | "system" | "down" | "up"),
        "alembic" => matches!(s, "upgrade" | "downgrade" | "stamp"),
        "prisma" => matches!(s, "migrate" | "db" | "generate"),
        "rm" | "del" | "rmdir" | "remove-item" | "mv" | "move-item" | "export" | "setx" | "chmod" | "touch" => true,
        _ => false,
    }
}

/// A command that kept working here (3+ successes) and has just started failing (1-3 failures
/// since its last success): what ran in this folder between the last success and the first
/// failure. None when that isn't the situation.
pub fn what_broke(st: &Store, entry: u32, cwd: u32) -> Option<Broke> {
    let mine: Vec<_> = st.execs.iter().filter(|x| x.entry == entry && x.cwd == Some(cwd) && x.exit.is_some()).collect();
    let last_ok = mine.iter().rposition(|x| x.exit == Some(0))?;
    let fails_since = mine.len() - 1 - last_ok;
    if fails_since == 0 || fails_since > 3 {
        return None;
    }
    let worked = mine[..=last_ok].iter().filter(|x| x.exit == Some(0)).count() as u32;
    if worked < 3 {
        return None;
    }
    let (t_ok, t_fail) = (mine[last_ok].ts, mine[last_ok + 1].ts);
    let mut seen: Vec<u32> = Vec::new();
    for x in st.execs.iter().filter(|x| x.cwd == Some(cwd) && x.ts >= t_ok && x.ts <= t_fail && x.entry != entry) {
        let e = &st.entries[x.entry as usize];
        if trivial(e) || !e.recallable(false) || seen.contains(&x.entry) {
            continue;
        }
        seen.push(x.entry);
    }
    let (mut between, rest): (Vec<u32>, Vec<u32>) = seen.into_iter().partition(|&i| changes_things(&st.entries[i as usize].gkey));
    between.extend(rest);
    let (ok, now) = (mine[last_ok], mine[mine.len() - 1]);
    Some(Broke { worked, last_ok: t_ok, first_fail: t_fail, between, ok_git: (ok.branch, ok.head), fail_git: (now.branch, now.head) })
}

// ---------------------------------------------------------------------------------------------
// runbook
// ---------------------------------------------------------------------------------------------

pub const SECTIONS: [&str; 7] = ["Set up", "Run", "Test", "Lint and format", "Build", "Database", "Deploy and release"];

/// Which runbook section a command belongs to, from what it runs (seeing through wrappers:
/// `docker compose exec web alembic upgrade head` is Database).
pub fn section_of(text: &str) -> Option<usize> {
    let inner = describe::wrapped(text);
    let t = inner.as_deref().unwrap_or(text).to_lowercase();
    let g = describe::group_key(&t);
    let mut w = g.split(' ');
    let (p, s) = (w.next().unwrap_or(""), w.next().unwrap_or(""));
    let has = |k: &str| t.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == ':')).any(|x| x == k);
    let script = |names: &[&str]| matches!(p, "npm" | "pnpm" | "yarn" | "bun" | "make" | "just" | "task") && names.iter().any(|n| has(n));
    // most specific first: a test or lint script is not "run"
    if matches!(p, "pytest" | "jest" | "vitest" | "mocha" | "rspec" | "phpunit" | "tox" | "nox")
        || (matches!(p, "go" | "cargo" | "dotnet" | "mvn" | "gradle" | "deno" | "bun") && s == "test")
        || (p == "python" && has("pytest"))
        || script(&["test", "test:unit", "test:e2e", "e2e", "coverage"])
        // `docker compose -f docker-compose.test.yml up`: the test suite, in containers (its
        // `down` only cleans up)
        || ((p == "docker" && s == "compose") || p == "docker-compose")
            && (has("up") || has("run"))
            && t.split_whitespace().any(|w| w.contains("test") && (w.ends_with(".yml") || w.ends_with(".yaml")))
    {
        return Some(2);
    }
    if matches!(p, "ruff" | "eslint" | "prettier" | "black" | "isort" | "flake8" | "mypy" | "pylint" | "stylelint" | "rubocop" | "golangci-lint" | "biome")
        || (p == "cargo" && matches!(s, "clippy" | "fmt"))
        || (p == "go" && matches!(s, "vet" | "fmt"))
        || (p == "tsc" && has("--noemit"))
        || script(&["lint", "format", "fmt", "typecheck", "check-types"])
    {
        return Some(3);
    }
    if matches!(p, "alembic" | "psql" | "mysql" | "sqlite3" | "mongosh" | "redis-cli" | "flyway" | "liquibase")
        || (p == "prisma" || (p == "npx" && has("prisma")))
        || (p == "python" && (has("migrate") || has("makemigrations")))
        || (p == "rails" && t.contains("db:"))
        || has("knex") && has("migrate")
    {
        return Some(5);
    }
    if matches!(p, "kubectl" | "helm" | "terraform" | "tofu" | "eas" | "vercel" | "netlify" | "fly" | "flyctl" | "heroku" | "gcloud" | "az" | "serverless" | "twine")
        || (p == "docker" && s == "push")
        || (matches!(p, "npm" | "cargo" | "pnpm" | "yarn") && s == "publish")
        || (p == "gh" && s == "release")
        || script(&["deploy", "release", "publish"])
    {
        return Some(6);
    }
    if (matches!(p, "npm" | "pnpm" | "yarn" | "bun") && matches!(s, "install" | "i" | "ci"))
        || (p == "yarn" && s.is_empty())
        || (matches!(p, "pip" | "gem" | "composer" | "bundle") && s == "install")
        || (matches!(p, "uv" | "poetry" | "pipenv" | "pdm") && matches!(s, "sync" | "install"))
        || matches!(p, "venv" | "virtualenv") // `python -m venv .venv`, seen through
        || (p == "python" && t.contains("-m venv"))
        || t.contains("activate")
        || (p == "go" && s == "mod")
        || (p == "dotnet" && s == "restore")
        || script(&["setup", "bootstrap", "install"])
    {
        return Some(0);
    }
    if (matches!(p, "npm" | "pnpm" | "yarn" | "bun") && (s == "build" || script(&["build"])))
        || (matches!(p, "cargo" | "go" | "dotnet" | "mvn" | "gradle") && matches!(s, "build" | "package" | "install"))
        || (p == "docker" && s == "build")
        || ((p == "docker" && s == "compose" || p == "docker-compose") && has("build"))
        || (p == "tsc" && !has("--noemit"))
        || (p == "make" && s.is_empty())
    {
        return Some(4);
    }
    if matches!(p, "uvicorn" | "gunicorn" | "flask" | "nodemon" | "streamlit" | "hypercorn" | "daphne")
        || (matches!(p, "npm" | "pnpm" | "yarn" | "bun") && (s == "start" || script(&["dev", "start", "serve", "preview", "watch"])))
        || (matches!(p, "cargo" | "go" | "dotnet" | "deno") && s == "run")
        || ((p == "docker" && s == "compose" || p == "docker-compose") && has("up"))
        || (p == "python" && (has("runserver") || has("uvicorn") || has("flask")))
        || (p == "rails" && matches!(s, "s" | "server"))
        || (matches!(p, "expo" | "npx") && has("expo") && has("start"))
    {
        return Some(1);
    }
    None
}

/// One command of the runbook.
pub struct Line {
    /// the recorded line it was run in (the latest), for who may see it; None for a command the
    /// project's own files declare (`npm test` from package.json) and nobody ran yet
    pub entry: Option<u32>,
    /// what it ran, as typed, with the environment it needs (`. .\msvc-env.ps1; cargo build`)
    pub text: String,
    /// the folder it runs in, from the project root: `.` or `frontend`
    pub dir: String,
    /// ...and as a path
    pub abs: String,
    /// its record: every time it ran there, however it was wrapped
    pub agg: Agg,
    /// the file that declares it, when it comes from one (`package.json`, `Makefile`, `pytest.ini`)
    pub declared: Option<&'static str>,
    /// a test or lint run on some files only (`pytest app/tests/test_x.py`)
    pub partial: bool,
}

pub struct Runbook {
    /// per SECTIONS entry, the folder asked about first, then most used
    pub sections: Vec<Vec<Line>>,
    /// usual sequences here, most repeated first: (entry, command) per step, and how often
    pub flows: Vec<(Vec<(u32, String)>, u32)>,
    /// commands done repeatedly here that no section rule knows (`python scripts/seed_db.py`,
    /// `./dev.sh`): for a language model to place, most used first
    pub unplaced: Vec<Line>,
    /// the folder asked about, from the project root
    pub here: String,
}

/// Where in the project a folder is, from its root (`.`, `frontend`), or None when the folder
/// isn't part of the project. Remembered per folder: it looks at the disk.
struct Places<'a> {
    st: &'a Store,
    scope: Scope,
    seen: HashMap<String, Option<String>>,
}

impl Places<'_> {
    fn of(&mut self, dir: &str) -> Option<String> {
        if let Some(v) = self.seen.get(dir) {
            return v.clone();
        }
        let v = match self.scope {
            // the project's own folder, or one under it
            Scope::Folder(f) => rel_to(dir, self.st.cwd_name(f)),
            // any checkout of the repo (a worktree is its own), from that checkout's root
            Scope::Repo(_) if self.st.scope_repo(dir) == self.scope => Some(checkout_root(dir).and_then(|r| rel_to(dir, &r)).unwrap_or_else(|| ".".into())),
            _ => None,
        };
        self.seen.insert(dir.to_string(), v.clone());
        v
    }
}

/// The root of the checkout a folder is in: where `.git` is (a folder, or a worktree's file).
fn checkout_root(dir: &str) -> Option<String> {
    std::path::Path::new(dir).ancestors().find(|p| p.join(".git").exists()).map(|p| p.to_string_lossy().into_owned())
}

/// What a folder's own files say its tasks are: package.json scripts (run with the package
/// manager its lockfile names), Makefile targets, pytest's config. (section, command, file),
/// for the tasks a runbook section knows.
fn declared(dir: &str) -> Vec<(usize, String, &'static str)> {
    let p = std::path::Path::new(dir);
    let read = |f: &str| std::fs::read_to_string(p.join(f)).ok();
    let mut out = Vec::new();
    if let Some(v) = read("package.json").and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok()) {
        let pm = [("pnpm-lock.yaml", "pnpm"), ("yarn.lock", "yarn"), ("bun.lockb", "bun"), ("bun.lock", "bun")]
            .iter()
            .find(|(lock, _)| p.join(lock).exists())
            .map_or("npm", |(_, pm)| pm);
        for name in v["scripts"].as_object().into_iter().flatten().map(|(k, _)| k.as_str()) {
            let cmd = match (pm, name) {
                (_, "test" | "start") => format!("{pm} {name}"),
                ("yarn", _) => format!("yarn {name}"),
                _ => format!("{pm} run {name}"),
            };
            if let Some(s) = section_of(&cmd) {
                out.push((s, cmd, "package.json"));
            }
        }
    }
    // `test:` and `build: deps`; not `X := 1`, `.PHONY:` or a recipe line
    for line in read("Makefile").or_else(|| read("makefile")).unwrap_or_default().lines() {
        let Some((t, rest)) = line.split_once(':') else { continue };
        let name = t.starts_with(|c: char| c.is_ascii_alphabetic()) && t.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
        if name && !rest.starts_with('=') {
            let cmd = format!("make {t}");
            if let Some(s) = section_of(&cmd) {
                out.push((s, cmd, "Makefile"));
            }
        }
    }
    let pytest = ["pytest.ini", "conftest.py"]
        .into_iter()
        .find(|f| p.join(f).exists())
        .or_else(|| read("pyproject.toml").filter(|t| t.contains("[tool.pytest")).map(|_| "pyproject.toml"));
    if let Some(f) = pytest {
        out.push((2, "pytest".to_string(), f));
    }
    out
}

/// `dir` from `root`: `.` for the root itself, `web/src` under it, None outside it.
fn rel_to(dir: &str, root: &str) -> Option<String> {
    let norm = |p: &str| p.replace('\\', "/").trim_end_matches('/').to_string();
    let (d, r) = (norm(dir), norm(root));
    if d.eq_ignore_ascii_case(&r) {
        return Some(".".into());
    }
    let under = d.len() > r.len() + 1 && d.as_bytes()[r.len()] == b'/' && d[..r.len()].eq_ignore_ascii_case(&r);
    under.then(|| d[r.len() + 1..].to_string())
}

/// Variants that are the same step of a runbook: they differ only in how the program is reached
/// (`npx tsc` / `node_modules/.bin/tsc`), in data (paths, numbers, messages) or in switches
/// without a value (`expo start --clear` / `-c` / `--tunnel`). Options with a value stay:
/// `eas build --profile production` is not `--profile staging`.
fn fold_key(main: &str) -> String {
    let inner = describe::wrapped(main);
    let src = inner.as_deref().unwrap_or(main);
    // the program by name (`tsc`, `python`), however it was reached; then its arguments, without
    // options that only name what it uses anyway (`tsc -p tsconfig.json` is `tsc`)
    let prog = describe::parse_cmd(src).0.unwrap_or_default();
    let w: Vec<&str> = src.split_whitespace().collect();
    let mut kept: Vec<&str> = Vec::with_capacity(w.len());
    let mut i = 0;
    while i < w.len() {
        let (flag, val) = w[i].split_once('=').map_or((w[i], None), |(f, v)| (f, Some(v)));
        match val.or_else(|| w.get(i + 1).copied()) {
            Some(v) if flag.starts_with('-') && is_default(&prog, flag, v) => i += if val.is_some() { 1 } else { 2 },
            _ => {
                kept.push(w[i]);
                i += 1;
            }
        }
    }
    let shape = describe::shape_key(&kept.join(" "));
    let t: Vec<&str> = shape.split_whitespace().collect();
    let start = t.iter().position(|w| *w != "&").map_or(t.len(), |i| i + 1);
    let switch = |i: usize| t[i].starts_with('-') && !t[i].contains('=') && t.get(i + 1).is_none_or(|n| n.starts_with('-'));
    let args = (start..t.len()).filter(|&i| !switch(i) && !matches!(t[i], "_path" | "_n" | "\"_\"")).map(|i| t[i]);
    std::iter::once(prog.as_str()).chain(args).collect::<Vec<_>>().join(" ")
}

/// An option that names what the tool uses anyway: `tsc -p tsconfig.json` (or `-p .`),
/// `docker compose -f docker-compose.yml`, `jest --config jest.config.js`.
fn is_default(prog: &str, flag: &str, value: &str) -> bool {
    let v = value.trim_matches(['"', '\'']).replace('\\', "/").to_lowercase();
    let v = v.strip_prefix("./").unwrap_or(&v);
    let config = |names: &[&str]| names.iter().any(|n| v.starts_with(n));
    match (prog, flag) {
        ("tsc" | "vue-tsc", "-p" | "--project") => matches!(v, "." | "" | "tsconfig.json"),
        ("docker" | "docker-compose", "-f" | "--file") => matches!(v, "docker-compose.yml" | "docker-compose.yaml" | "compose.yml" | "compose.yaml"),
        ("cargo", "--manifest-path") => v == "cargo.toml",
        ("make", "-f" | "--file") => matches!(v, "makefile" | "gnumakefile"),
        ("pytest", "-c") => matches!(v, "pytest.ini" | "pyproject.toml" | "setup.cfg" | "tox.ini"),
        ("jest", "-c" | "--config") => config(&["jest.config."]),
        ("vitest" | "vite", "-c" | "--config") => config(&["vitest.config.", "vite.config."]),
        ("eslint", "-c" | "--config") => config(&["eslint.config.", ".eslintrc"]),
        ("prettier", "--config") => config(&["prettier.config.", ".prettierrc"]),
        ("playwright", "-c" | "--config") => config(&["playwright.config."]),
        ("webpack", "-c" | "--config") => config(&["webpack.config."]),
        _ => false,
    }
}

/// Its arguments carry no data (paths, numbers, messages): the general form of a step.
fn general(main: &str) -> bool {
    !describe::shape_key(main).split_whitespace().skip(1).any(|t| matches!(t, "_path" | "_n" | "\"_\""))
}

/// A command that does the same from any folder: it names absolute paths (a venv's activate) or
/// works on a running container (`docker exec db psql ...`). It gets no folder of its own.
fn anywhere(main: &str) -> bool {
    let abs = |a: &str| {
        let b = a.trim_matches(['"', '\'']).as_bytes();
        b.len() > 2 && b[0].is_ascii_alphabetic() && b[1] == b':' && matches!(b[2], b'\\' | b'/')
    };
    let g = describe::group_key(main);
    main.split_whitespace().any(abs) || g == "docker exec" || g.starts_with("kubectl") || g == "ssh"
}

/// A line that only makes sense once: it runs something from a temporary folder (an agent's
/// scratch script), not something the project has.
fn throwaway(text: &str) -> bool {
    let l = text.to_lowercase().replace('\\', "/");
    ["/temp/", "/tmp/", "$temp", "$env:temp", "%temp%", "scratchpad"].iter().any(|k| l.contains(k))
}

/// The folder of a command that runs the same from any (see `anywhere`).
const ANYWHERE: &str = "*";

/// How this project is run: per section, the commands that worked here (run more than once, by
/// you or an agent, and worked more often than not), each as it really ran once unwrapped from
/// the `cd`s and output filters around it, with the folder it runs in; variants folded; the
/// folder asked about first. And the usual sequences in that folder.
pub fn runbook(st: &Store, scope: Scope, cwd: &str, per_section: usize) -> Runbook {
    let mut places = Places { st, scope, seen: HashMap::new() };
    // (folder, command) -> its record, the latest line it ran in, the command alone, the folder's path
    let mut runs: HashMap<(String, String), (Agg, u32, String, String)> = HashMap::new();
    for (i, e) in st.entries.iter().enumerate() {
        if !e.recallable(false) || fragment(&e.text) || !st.in_scope(e, scope) {
            continue;
        }
        let Some(cmds) = unwrap::commands(&e.text) else { continue };
        for r in e.rows.iter().filter(|r| st.row_in_scope(r, scope) && !st.row_unscoped(r)) {
            let Some(base) = r.cwd.map(|c| st.cwd_name(c)) else { continue };
            for c in cmds.iter().filter(|c| !trivial_cmd(&c.main) && !throwaway(&c.text)) {
                let Some(abs) = unwrap::resolve_dir(base, &c.cds) else { continue };
                let Some(dir) = places.of(&abs) else { continue };
                let dir = if anywhere(&c.main) { ANYWHERE.to_string() } else { dir };
                // a run counts; whether it worked, as far as the line's exit status is its own, and
                // then as far as what it printed said (verdict.rs): a hidden result, the step of
                // `a && b` that failed, a step that never ran
                let d = st.seen.get(&(i as u32, r.cwd, c.text.clone())).copied().unwrap_or_default();
                let add = |n: u32, dn: i32| (i64::from(n) + i64::from(dn)).max(0) as u32;
                let own_ok = if c.outcome >= Outcome::OnSuccess { r.ok } else { 0 };
                let own_fail = if c.outcome == Outcome::Own { r.fail } else { 0 };
                let (n, ok, fail) = (add(r.runs, d[0]), add(own_ok, d[1]), add(own_fail, d[2]));
                if n == 0 {
                    continue;
                }
                let (a, latest, _, at) = runs.entry((dir, c.text.clone())).or_insert_with(|| (Agg::default(), i as u32, c.main.clone(), abs.clone()));
                a.runs += n;
                a.ok += ok;
                a.fail += fail;
                a.human += r.human;
                a.agent += r.agent;
                if r.last_used >= a.last_used {
                    (a.last_used, a.last_exit, a.last_actor, *latest, *at) = (r.last_used, r.last_exit, r.last_actor.clone(), i as u32, abs);
                }
            }
        }
    }
    // a runbook is what's done repeatedly (by you or an agent) and works: a one-off (`npm
    // install left-pad`) isn't how the project is set up. What was only ever run with its result
    // hidden (`npx tsc | wc -l`) is in when it was run often, without a claim that it works.
    // Then one line per step: the variant without data (`npx jest` over `npx jest src/a.test.ts`),
    // else the one run most.
    let mut steps: HashMap<(Option<usize>, String, String), (Line, bool)> = HashMap::new();
    for ((dir, text), (agg, entry, main, abs)) in runs {
        let unseen = agg.runs.saturating_sub(agg.ok + agg.fail);
        if agg.runs < 2 || agg.ok < agg.fail || (agg.ok == 0 && unseen < 3) {
            continue;
        }
        let sec = section_of(&main);
        // with no section, only one line and short: something a person would put in a runbook
        if sec.is_none() && (text.contains('\n') || text.len() > 120) {
            continue;
        }
        let plain = general(&main);
        let key = (sec, dir.clone(), fold_key(&main));
        let partial = matches!(sec, Some(2 | 3)) && describe::shape_key(&main).split_whitespace().skip(1).any(|t| t == "_path");
        let line = Line { entry: Some(entry), text, dir, abs, agg, declared: None, partial };
        let better = |old: &(Line, bool)| (plain, line.agg.runs, line.agg.last_used) > (old.1, old.0.agg.runs, old.0.agg.last_used);
        if steps.get(&key).is_none_or(better) {
            steps.insert(key, (line, plain));
        }
    }
    let here = places.of(cwd).unwrap_or_else(|| ".".into());
    let order = |a: &Line, b: &Line| {
        (b.dir == here, b.agg.runs, b.agg.last_used, &a.text).cmp(&(a.dir == here, a.agg.runs, a.agg.last_used, &b.text))
    };
    let mut sections: Vec<Vec<Line>> = (0..SECTIONS.len()).map(|_| Vec::new()).collect();
    let mut unplaced: Vec<Line> = Vec::new();
    for ((sec, _, _), (mut line, _)) in steps {
        // runs the same from any folder: from the one asked about
        if line.dir == ANYWHERE {
            line.dir = here.clone();
        }
        match sec {
            Some(s) => sections[s].push(line),
            None => unplaced.push(line),
        }
    }
    // what the project's own files declare, for a task the history has no whole command for
    // (none, or only one-file test runs): `npm test` from package.json, `make lint`, `pytest`
    let root = match scope {
        Scope::Folder(f) => st.cwd_name(f).to_string(),
        _ => checkout_root(cwd).unwrap_or_else(|| cwd.to_string()),
    };
    let mut dirs: Vec<String> = sections.iter().flatten().map(|l| l.dir.clone()).chain(std::iter::once(here.clone())).collect();
    dirs.sort();
    dirs.dedup();
    for dir in dirs {
        let abs = if dir == "." { root.clone() } else { std::path::Path::new(&root).join(&dir).to_string_lossy().into_owned() };
        for (sec, text, file) in declared(&abs) {
            if !sections[sec].iter().any(|l| l.dir == dir && (!l.partial || l.text == text)) {
                sections[sec].push(Line { entry: None, text, dir: dir.clone(), abs: abs.clone(), agg: Agg::default(), declared: Some(file), partial: false });
            }
        }
    }
    // `per_section` per folder: one app's many commands never crowd out another's
    for s in sections.iter_mut() {
        s.sort_by(&order);
        let mut per: HashMap<String, usize> = HashMap::new();
        s.retain(|l| {
            let n = per.entry(l.dir.clone()).or_default();
            *n += 1;
            *n <= per_section
        });
        s.truncate(per_section * 2 + 2);
    }
    unplaced.sort_by(&order);
    unplaced.truncate(30);
    Runbook { sections, flows: usual_sequences(st, cwd), unplaced, here }
}

/// The usual sequences in this folder, each step as it really ran, unwrapped. A sequence with a
/// step that isn't one plain command here (it moved to another folder, or ran several) is left out.
fn usual_sequences(st: &Store, cwd: &str) -> Vec<(Vec<(u32, String)>, u32)> {
    let Some(folder) = st.cwd_index(cwd) else { return Vec::new() };
    let base = st.cwd_name(folder);
    let step = |i: u32| -> Option<(u32, String)> {
        let e = &st.entries[i as usize];
        if trivial(e) {
            return None;
        }
        let cmds = unwrap::commands(&e.text)?;
        let [c] = &cmds[..] else { return None };
        let stays = c.cds.is_empty() || unwrap::resolve_dir(base, &c.cds).is_some_and(|d| rel_to(&d, base).as_deref() == Some("."));
        stays.then(|| (i, c.text.clone()))
    };
    let mut out: Vec<(Vec<(u32, String)>, u32)> = Vec::new();
    for f in flows::detect(st, Some(folder), 2, 5, 300, 60) {
        let Some(mut steps) = f.seq.iter().map(|&i| step(i)).collect::<Option<Vec<_>>>() else { continue };
        // two wrappers of one command in a row are one step
        steps.dedup_by(|a, b| a.1 == b.1);
        let same = |o: &(Vec<(u32, String)>, u32)| o.0.iter().map(|s| &s.1).eq(steps.iter().map(|s| &s.1));
        if steps.len() >= 2 && !out.iter().any(same) {
            out.push((steps, f.count));
        }
        if out.len() == 3 {
            break;
        }
    }
    out
}

/// The runbook as JSON. `show` gives the text to show for a command (of the recorded line it
/// ran in, or None for one a project file declares; in the folder given), or None to leave it
/// out (for an agent: outside its folders, or still secret-looking after redaction).
pub fn runbook_json(st: &Store, scope: Scope, cwd: &str, show: &dyn Fn(Option<u32>, &str, &str) -> Option<String>, with_unplaced: bool) -> serde_json::Value {
    use serde_json::json;
    let now = crate::config::now();
    let rb = runbook(st, scope, cwd, 5);
    let line = |l: &Line| {
        let t = show(l.entry, &l.text, &l.abs)?;
        if let Some(f) = l.declared {
            return Some(json!({"command": t, "dir": l.dir, "declared": f, "runs": 0, "worked": 0, "failed": 0, "unseen": 0}));
        }
        // runs = worked + failed + unseen (its result hidden by a pipe, or never recorded)
        let unseen = l.agg.runs.saturating_sub(l.agg.ok + l.agg.fail);
        let mut v = json!({"command": t, "dir": l.dir, "runs": l.agg.runs, "worked": l.agg.ok, "failed": l.agg.fail, "unseen": unseen, "last_run": ago(l.agg.last_used, now)});
        if l.partial {
            v["partial"] = json!(true);
        }
        Some(v)
    };
    let sections: Vec<_> = SECTIONS
        .iter()
        .zip(&rb.sections)
        .filter_map(|(title, cmds)| {
            let cmds: Vec<_> = cmds.iter().filter_map(line).collect();
            (!cmds.is_empty()).then(|| json!({"title": title, "commands": cmds}))
        })
        .collect();
    let flows: Vec<_> = rb
        .flows
        .iter()
        .filter_map(|(seq, count)| {
            let steps: Option<Vec<String>> = seq.iter().map(|(i, t)| show(Some(*i), t, cwd)).collect();
            steps.map(|s| json!({"steps": s, "count": count}))
        })
        .collect();
    let mut out = json!({"found": !sections.is_empty() || !flows.is_empty(), "folder": cwd, "here": rb.here,
           "sections": sections, "flows": flows, "generated": false});
    if with_unplaced {
        out["unplaced"] = json!(rb.unplaced.iter().filter_map(line).collect::<Vec<_>>());
    }
    out
}


/// "2 hours ago", "yesterday", "3 weeks ago": for sentences, not columns.
pub fn ago(ts: i64, now: i64) -> String {
    let s = (now - ts).max(0);
    let (n, unit) = match s {
        s if s < 3600 => return "just now".into(),
        s if s < 86400 => (s / 3600, "hour"),
        s if s < 2 * 86400 => return "yesterday".into(),
        s if s < 14 * 86400 => (s / 86400, "day"),
        s if s < 60 * 86400 => (s / (7 * 86400), "week"),
        s if s < 365 * 86400 => (s / (30 * 86400), "month"),
        s => (s / (365 * 86400), "year"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

/// A duration as people say it: `11s`, `3m 12s`, `1h 5m`; `about` rounds to one unit (`about 2m`).
pub fn took(ms: u32, about: bool) -> String {
    let s = (ms as u64 + 500) / 1000;
    match (s, about) {
        (0, _) => format!("{:.1}s", ms as f64 / 1000.0),
        (s, _) if s < 60 => format!("{s}s"),
        (s, true) if s < 3600 => format!("{}m", (s + 30) / 60),
        (s, false) if s < 3600 => if s % 60 == 0 { format!("{}m", s / 60) } else { format!("{}m {}s", s / 60, s % 60) },
        (s, true) => format!("{}h", (s + 1800) / 3600),
        (s, false) => if (s % 3600) / 60 == 0 { format!("{}h", s / 3600) } else { format!("{}h {}m", s / 3600, (s % 3600) / 60) },
    }
}

/// A command as one short line: first line, at most `n` characters.
pub fn short(text: &str, n: usize) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    if line.chars().count() > n {
        format!("{}…", line.chars().take(n - 1).collect::<String>())
    } else if text.trim().contains('\n') {
        format!("{line} …")
    } else {
        line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections() {
        for (cmd, want) in [
            ("npm install", Some(0)),
            ("python -m venv .venv", Some(0)),
            ("uv sync", Some(0)),
            ("npm run dev", Some(1)),
            ("uvicorn app.main:app --reload", Some(1)),
            ("docker compose up -d", Some(1)),
            ("pytest -x -q", Some(2)),
            ("npm test", Some(2)),
            ("npm run test:e2e", Some(2)),
            ("ruff check --fix .", Some(3)),
            ("npm run lint", Some(3)),
            ("npm run build", Some(4)),
            ("docker compose build --no-cache", Some(4)),
            ("alembic upgrade head", Some(5)),
            ("docker compose exec web alembic upgrade head", Some(5)),
            ("npx prisma migrate dev", Some(5)),
            ("kubectl rollout restart deploy/api -n api", Some(6)),
            ("git status", None),
            ("ssh deploy@host", None),
            (".venv/Scripts/python.exe e2e/tui_test.py", None),
            ("docker compose -f docker-compose.test.yml up --build --abort-on-container-exit", Some(2)),
            ("docker compose -f docker-compose.prod.yml up -d", Some(1)),
            ("docker compose -f docker-compose.test.yml down -v", None),
        ] {
            assert_eq!(section_of(cmd), want, "{cmd}");
        }
    }

    #[test]
    fn looking_around() {
        let t = |s: &str| unwrap::commands(s).is_none_or(|cs| cs.iter().all(|c| trivial_cmd(&c.main)));
        assert!(t(r"cd d:\PlanetNaidu\planet_naidu_api"), "a cd is not a program named after the folder");
        assert!(t("cd api; git status"));
        assert!(t(r#"try { . "c:\x\resources\app\out\vs\workbench\contrib\terminal\common\scripts\shellIntegration.ps1" } catch {}"#));
        assert!(!t("cd api && npm test"));
        assert!(!t("npm test"));
        assert!(t("npx vercel --version") && t("go version"));
        assert!(!t("pytest -v"));
        assert!(fragment("from PIL import Image") && fragment("python - <<'PY'"));
        assert!(!fragment(r". .\msvc-env.ps1; cargo build --release") && !fragment("python - <<'PY'\nprint(1)\nPY"));
        assert!(!fragment(".venv/Scripts/python.exe -m pytest -q"));
    }

    #[test]
    fn what_broke_says_what_git_changed() {
        let story = |fail_branch: &str, fail_head: &str| {
            let db = rusqlite::Connection::open_in_memory().unwrap();
            crate::db::migrate(&db).unwrap();
            let mut s = Store::default();
            let at = |cmd: &str, exit: i64, ts: i64, branch: &str, head: &str| crate::db::Run {
                cmd: cmd.into(), exit: Some(exit), cwd: Some("C:/app".into()), session: "s".into(), actor: "human".into(), ts,
                branch: Some(branch.into()), head: Some(head.into()), ..Default::default()
            };
            let runs = [
                at("npm test", 0, 1, "main", "aaaaaaaaaaaa"),
                at("npm test", 0, 2, "main", "aaaaaaaaaaaa"),
                at("npm test", 0, 3, "main", "aaaaaaaaaaaa"),
                at("git pull", 0, 4, "main", "aaaaaaaaaaaa"),
                at("npm test", 1, 5, fail_branch, fail_head),
            ];
            for r in &runs {
                let rec = crate::db::record_run(&db, r).unwrap();
                s.apply_run(rec.command_id, rec.new_row, r, Some((&vec![1.0; crate::config::DIM], None, &[])));
            }
            let (i, _) = s.entry("npm test").unwrap();
            let b = what_broke(&s, i, s.cwd_index("C:/app").unwrap()).unwrap();
            GitChange::of(&s, &b)
        };
        assert_eq!(story("feat/x", "bbbbbbbbbbbb"), Some(GitChange::Branch { worked: "main".into(), fails: "feat/x".into() }));
        assert_eq!(story("main", "bbbbbbbbbbbb"), Some(GitChange::Commits { worked: "aaaaaaa".into(), fails: "bbbbbbb".into() }));
        assert_eq!(story("main", "aaaaaaaaaaaa"), Some(GitChange::SameCommit));
    }

    #[test]
    fn typical_time_and_what_is_costly() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::migrate(&db).unwrap();
        let mut s = Store::default();
        let mut ts = 0;
        let mut add = |s: &mut Store, cmd: &str, exit: i64, ms: i64, cwd: &str| {
            ts += 1;
            let r = crate::db::Run { cmd: cmd.into(), exit: Some(exit), cwd: Some(cwd.into()), session: "s".into(), actor: "human".into(), ts, duration_ms: Some(ms), ..Default::default() };
            let rec = crate::db::record_run(&db, &r).unwrap();
            s.apply_run(rec.command_id, rec.new_row, &r, Some((&vec![1.0; crate::config::DIM], None, &[])));
        };
        for ms in [100_000, 120_000, 200_000] {
            add(&mut s, "cargo build --release", 0, ms, "C:/app");
        }
        add(&mut s, "cargo build --release", 0, 5_000, "C:/other");
        for _ in 0..2 {
            add(&mut s, "gti status", 1, 40, "C:/app");
            add(&mut s, "make e2e", 1, 15_000, "C:/app");
            add(&mut s, "npm install left-pad", 1, 900, "C:/app");
        }
        let c = s.cwd_index("C:/app").unwrap();
        let (b, e) = s.entry("cargo build --release").unwrap();
        assert_eq!(s.typical(e, Scope::Folder(c), true), Some(120_000), "the median here");
        assert_eq!(s.typical(e, Scope::All, false), None, "it never failed");
        let _ = b;
        let costly_here = |cmd: &str| costly(&s, s.entry(cmd).unwrap().0, c);
        assert!(!costly_here("gti status"), "fails in a blink");
        assert!(costly_here("make e2e"), "its failures take 15s");
        assert!(costly_here("npm install left-pad"), "an install changes things, however fast it fails");
        assert_eq!(took(120_000, false), "2m");
        assert_eq!(took(192_000, false), "3m 12s");
        assert_eq!(took(150_000, true), "3m");
        assert_eq!(took(11_000, true), "11s");
        assert_eq!(took(3_900_000, false), "1h 5m");
    }

    #[test]
    fn warns_only_on_strong_evidence() {
        let warn = |exits: &[i64]| {
            let db = rusqlite::Connection::open_in_memory().unwrap();
            crate::db::migrate(&db).unwrap();
            let mut s = Store::default();
            for (k, &x) in exits.iter().enumerate() {
                let r = crate::db::Run { cmd: "npm ci".into(), exit: Some(x), cwd: Some("C:/app".into()), session: "s".into(), actor: "human".into(), ts: k as i64 + 1, ..Default::default() };
                let rec = crate::db::record_run(&db, &r).unwrap();
                s.apply_run(rec.command_id, rec.new_row, &r, Some((&vec![1.0; crate::config::DIM], None, &[])));
            }
            let (i, _) = s.entry("npm ci").unwrap();
            warning(&s, i, s.cwd_index("C:/app").unwrap())
        };
        assert_eq!(warn(&[1, 1]), Some(Warning { failed: 2, worked: 0 }));
        assert_eq!(warn(&[1]), None, "one failure is not a pattern");
        assert_eq!(warn(&[0, 0, 1, 1, 1]), Some(Warning { failed: 3, worked: 2 }));
        assert_eq!(warn(&[0, 1, 1]), None, "it worked here, and only 2 failures since");
        assert_eq!(warn(&[1, 1, 1, 0]), None, "it just worked");
    }

    #[test]
    fn flaky_only_when_it_keeps_flipping_with_nothing_changed() {
        // (exit, commit) per run of `npm test`; "+" in the commit slot runs `npm install` before it
        let flaky_of = |runs: &[(i64, &str)]| {
            let db = rusqlite::Connection::open_in_memory().unwrap();
            crate::db::migrate(&db).unwrap();
            let mut s = Store::default();
            let mut ts = 0;
            let mut add = |s: &mut Store, cmd: &str, exit: i64, head: &str| {
                ts += 1;
                let r = crate::db::Run { cmd: cmd.into(), exit: Some(exit), cwd: Some("C:/app".into()), session: "s".into(), actor: "human".into(), ts,
                                         head: (!head.is_empty()).then(|| head.to_string()), ..Default::default() };
                let rec = crate::db::record_run(&db, &r).unwrap();
                s.apply_run(rec.command_id, rec.new_row, &r, Some((&vec![1.0; crate::config::DIM], None, &[])));
            };
            for &(exit, head) in runs {
                let head = if let Some(h) = head.strip_prefix('+') { add(&mut s, "npm install", 0, h); h } else { head };
                add(&mut s, "npm test", exit, head);
            }
            let (i, _) = s.entry("npm test").unwrap();
            flaky(&s, i, s.cwd_index("C:/app").unwrap())
        };
        let a = "aaaaaaaaaaaa";
        let flipping: Vec<(i64, &str)> = [0, 1, 0, 0, 1, 0, 1, 0].iter().map(|&e| (e, a)).collect();
        assert_eq!(flaky_of(&flipping), Some(Flaky { worked: 5, runs: 8 }));
        let dev_loop: Vec<(i64, &str)> = [1, 1, 0, 0, 0, 1, 0, 0, 0, 0].iter().map(|&e| (e, a)).collect();
        assert_eq!(flaky_of(&dev_loop), None, "failed, fixed, broke, fixed: not flaky");
        let committed: Vec<(i64, &str)> = [(0, "c1"), (1, "c2"), (0, "c3"), (1, "c4"), (0, "c5"), (1, "c6"), (0, "c7")].to_vec();
        assert_eq!(flaky_of(&committed), None, "every flip came with a new commit");
        let installs: Vec<(i64, &str)> = [(0, a), (1, "+aaaaaaaaaaaa"), (0, "+aaaaaaaaaaaa"), (1, "+aaaaaaaaaaaa"), (0, "+aaaaaaaaaaaa"), (1, "+aaaaaaaaaaaa")].to_vec();
        assert_eq!(flaky_of(&installs), None, "an install ran before every flip");
        assert_eq!(flaky_of(&[(0, a), (1, a), (0, a), (1, a)]), None, "too few runs to tell");
    }

    #[test]
    fn folders_from_the_project_root() {
        assert_eq!(rel_to(r"D:\app", r"D:\app").as_deref(), Some("."));
        assert_eq!(rel_to(r"D:\app\frontend", r"d:\APP").as_deref(), Some("frontend"));
        assert_eq!(rel_to(r"D:\app\web\src", r"D:\app\").as_deref(), Some("web/src"));
        assert_eq!(rel_to(r"D:\app2", r"D:\app"), None);
        assert_eq!(rel_to(r"D:\other", r"D:\app"), None);
    }

    #[test]
    fn variants_that_are_one_step() {
        let k = fold_key;
        assert_eq!(k("npx expo start --clear"), k("npx expo start -c"));
        assert_eq!(k("npx expo start --clear"), k("npx expo start --clear --tunnel"));
        assert_eq!(k("npx jest --silent"), k("npx jest src/a.test.ts"));
        assert_eq!(k("cargo build --release"), k("cargo build"));
        assert_eq!(k("npx tsc --noEmit"), k("node_modules/.bin/tsc --noEmit"));
        assert_eq!(k("npx tsc --noEmit"), k("./node_modules/.bin/tsc --noEmit"));
        assert_ne!(k("npx tsc --noEmit"), k("npx tsc --noEmit -p tsconfig.build.json"));
        // an option naming what the tool uses anyway
        assert_eq!(k("npx tsc --noEmit"), k("npx tsc --noEmit -p tsconfig.json"));
        assert_eq!(k("npx tsc --noEmit"), k("npx tsc --noEmit -p ."));
        assert_eq!(k("npx tsc --noEmit"), k("npx tsc --noEmit --project=./tsconfig.json"));
        assert_eq!(k("docker compose up -d"), k("docker compose -f docker-compose.yml up -d"));
        assert_ne!(k("docker compose up -d"), k("docker compose -f docker-compose.test.yml up -d"));
        assert_eq!(k("npx jest"), k("npx jest --config jest.config.js"));
        assert_ne!(k(".venv/Scripts/python.exe -m pytest"), k(".venv/Scripts/python.exe -m mypy"));
        assert!(general("node_modules/.bin/tsc --noEmit"));
        assert!(!general("npx jest src/a.test.ts"));
        assert_ne!(k("npx eas build --profile production --platform ios"), k("npx eas build --profile staging --platform android"));
        assert_ne!(k("docker compose exec web alembic upgrade head"), k("docker compose exec web alembic current"));
        assert_ne!(k("npm run dev"), k("npm run build"));
    }

    #[test]
    fn where_a_command_runs_does_not_matter() {
        assert!(anywhere(r"& d:\app\api\.venv\Scripts\Activate.ps1"));
        assert!(anywhere(r#"docker exec app_db psql -U app -tAc "SELECT 1""#));
        assert!(!anywhere("docker compose up -d"));
        assert!(!anywhere("npm install"));
        assert!(throwaway(r#".venv/Scripts/python.exe "C:/Users/me/AppData/Local/Temp/claude/x/scratchpad/t.py""#));
        assert!(throwaway("S=/tmp/x; python $S/a.py"));
        assert!(!throwaway("pytest tests/"));
    }

    #[test]
    fn what_changes_things() {
        assert!(changes_things("git pull"));
        assert!(changes_things("npm install"));
        assert!(!changes_things("npm test"));
        assert!(!changes_things("git status"));
    }

    #[test]
    fn ago_reads_as_a_sentence() {
        let now = 1_000_000_000;
        assert_eq!(ago(now - 60, now), "just now");
        assert_eq!(ago(now - 2 * 3600, now), "2 hours ago");
        assert_eq!(ago(now - 30 * 3600, now), "yesterday");
        assert_eq!(ago(now - 3 * 86400, now), "3 days ago");
        assert_eq!(ago(now - 21 * 86400, now), "3 weeks ago");
        assert_eq!(ago(now - 100 * 86400, now), "3 months ago");
    }

    #[test]
    fn short_lines() {
        assert_eq!(short("npm run dev", 40), "npm run dev");
        assert_eq!(short("python - <<'PY'\nprint(1)\nPY", 40), "python - <<'PY' …");
        assert_eq!(short("abcdefghij", 5), "abcd…");
    }
}
