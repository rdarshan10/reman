//! What the history says about a folder, beyond search:
//!  - `last_visit`: what you did here last time, for the "welcome back" line;
//!  - `what_broke`: a command that kept working here has started failing - what ran here since;
//!  - `runbook`: how this project is run, from the commands that worked here.
//! Pure functions over the in-memory store; the daemon, the CLI and MCP format the results.
use crate::describe;
use crate::flows::{self, Flow};
use crate::store::{Agg, Entry, Scope, Store};

/// Looking around, not doing: never part of "what you did here".
pub fn trivial(e: &Entry) -> bool {
    if e.comment || e.noise || e.self_ref {
        return true;
    }
    let g = e.gkey.as_str();
    let prog = g.split(' ').next().unwrap_or("");
    matches!(
        prog,
        "cd" | "ls" | "ll" | "dir" | "cls" | "clear" | "pwd" | "echo" | "cat" | "type" | "exit" | "history" | "code" | "explorer"
            | "start" | "open" | "less" | "more" | "head" | "tail" | "which" | "where" | "whoami" | "set-location" | "get-childitem"
            | "get-content" | "gci" | "sl" | "popd" | "pushd" | "man" | "help" | "tldr" | "reman" | "r" | "rr" | "tree" | "du" | "df"
    ) || matches!(g, "git status" | "git log" | "git diff" | "git branch" | "git show" | "git remote" | "docker ps" | "docker images")
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
    Some(Broke { worked, last_ok: t_ok, first_fail: t_fail, between })
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
        || (p == "python" && has("venv"))
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

pub struct Runbook {
    /// per SECTIONS entry: (entry, its record here), most used first
    pub sections: Vec<Vec<(u32, Agg)>>,
    /// usual sequences here, most repeated first
    pub flows: Vec<Flow>,
    /// commands done repeatedly here that no section rule knows (`python scripts/seed_db.py`,
    /// `./dev.sh`): for a language model to place, most used first
    pub unplaced: Vec<(u32, Agg)>,
}

/// How this project is run: per section, the commands that worked here (run more than once, by
/// you or an agent, and worked more often than not), variants folded, most used first; and the
/// usual sequences.
pub fn runbook(st: &Store, scope: Scope, cwd: Option<u32>, per_section: usize) -> Runbook {
    let mut best: std::collections::HashMap<(usize, &str), (u32, Agg)> = std::collections::HashMap::new();
    let mut unplaced: std::collections::HashMap<&str, (u32, Agg)> = std::collections::HashMap::new();
    for (i, e) in st.entries.iter().enumerate() {
        if !e.recallable(false) || trivial(e) || !st.in_scope(e, scope) {
            continue;
        }
        let a = st.agg(e, scope);
        // a runbook is what's done repeatedly (by you or an agent): a one-off (`npm install
        // left-pad`) isn't how the project is set up
        if a.ok == 0 || a.ok < a.fail || a.runs < 2 {
            continue;
        }
        let Some(sec) = section_of(&e.text) else {
            // one line, no heredocs: something a person would put in a runbook
            if !e.text.contains('\n') && e.text.len() <= 120 {
                let slot = unplaced.entry(e.shape.as_str()).or_insert((i as u32, a.clone()));
                if (a.runs, a.last_used) > (slot.1.runs, slot.1.last_used) {
                    *slot = (i as u32, a);
                }
            }
            continue;
        };
        let slot = best.entry((sec, e.shape.as_str())).or_insert((i as u32, a.clone()));
        if (a.runs, a.last_used) > (slot.1.runs, slot.1.last_used) {
            *slot = (i as u32, a);
        }
    }
    let mut sections: Vec<Vec<(u32, Agg)>> = vec![Vec::new(); SECTIONS.len()];
    for ((sec, _), v) in best {
        sections[sec].push(v);
    }
    for s in sections.iter_mut() {
        s.sort_by(|a, b| (b.1.runs, b.1.last_used, a.0).cmp(&(a.1.runs, a.1.last_used, b.0)));
        s.truncate(per_section);
    }
    let flows = flows::detect(st, cwd, 2, 5, 300, 60)
        .into_iter()
        .filter(|f| f.seq.len() >= 2 && f.seq.iter().all(|&i| !trivial(&st.entries[i as usize])))
        .take(3)
        .collect();
    let mut unplaced: Vec<(u32, Agg)> = unplaced.into_values().collect();
    unplaced.sort_by(|a, b| (b.1.runs, b.1.last_used, a.0).cmp(&(a.1.runs, a.1.last_used, b.0)));
    unplaced.truncate(30);
    Runbook { sections, flows, unplaced }
}

/// The runbook as JSON. `show` gives the text to show for an entry, or None to leave it out
/// (for an agent: outside its folders, or still secret-looking after redaction).
pub fn runbook_json(st: &Store, scope: Scope, folder: Option<u32>, show: &dyn Fn(u32) -> Option<String>, with_unplaced: bool) -> serde_json::Value {
    use serde_json::json;
    let now = crate::config::now();
    let rb = runbook(st, scope, folder, 5);
    let sections: Vec<_> = SECTIONS
        .iter()
        .zip(&rb.sections)
        .filter_map(|(title, cmds)| {
            let cmds: Vec<_> = cmds
                .iter()
                .filter_map(|(i, a)| {
                    show(*i).map(|t| json!({"command": t, "runs": a.runs, "worked": a.ok, "failed": a.fail, "last_run": ago(a.last_used, now)}))
                })
                .collect();
            (!cmds.is_empty()).then(|| json!({"title": title, "commands": cmds}))
        })
        .collect();
    let flows: Vec<_> = rb
        .flows
        .iter()
        .filter_map(|f| {
            let steps: Option<Vec<String>> = f.seq.iter().map(|&i| show(i)).collect();
            steps.map(|s| json!({"steps": s, "count": f.count}))
        })
        .collect();
    let mut out = json!({"found": !sections.is_empty() || !flows.is_empty(), "folder": folder.map(|c| st.cwd_name(c).to_string()),
           "sections": sections, "flows": flows, "generated": false});
    if with_unplaced {
        out["unplaced"] = json!(rb
            .unplaced
            .iter()
            .filter_map(|(i, a)| show(*i).map(|t| json!({"command": t, "runs": a.runs, "worked": a.ok, "failed": a.fail, "last_run": ago(a.last_used, now)})))
            .collect::<Vec<_>>());
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
        ] {
            assert_eq!(section_of(cmd), want, "{cmd}");
        }
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
